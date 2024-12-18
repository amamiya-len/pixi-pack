use std::{
    collections::{HashMap, HashSet},
    fs::FileTimes,
    path::{Path, PathBuf},
    sync::Arc,
};

use fxhash::FxHashMap;
use indicatif::HumanBytes;
use rattler_index::{package_record_from_conda, package_record_from_tar_bz2};
use tokio::{
    fs::{self, create_dir_all, File},
    io::AsyncWriteExt,
};

use anyhow::Result;
use futures::{stream, StreamExt, TryFutureExt, TryStreamExt};
use rattler_conda_types::{package::ArchiveType, ChannelInfo, PackageRecord, Platform, RepoData};
use rattler_lock::{CondaPackage, LockFile, Package};
use rattler_networking::{AuthenticationMiddleware, AuthenticationStorage};
use reqwest_middleware::ClientWithMiddleware;
use tokio_tar::Builder;
use walkdir::WalkDir;

use crate::{
    get_size, util::set_default_file_times, PixiPackMetadata, ProgressReporter,
    CHANNEL_DIRECTORY_NAME, PIXI_PACK_METADATA_PATH,
};
use anyhow::anyhow;

/// Options for packing a pixi environment.
#[derive(Debug, Clone)]
pub struct PackOptions {
    pub environment: String,
    pub platform: Platform,
    pub auth_file: Option<PathBuf>,
    pub output_file: PathBuf,
    pub manifest_path: PathBuf,
    pub metadata: PixiPackMetadata,
    pub injected_packages: Vec<PathBuf>,
    pub ignore_pypi_errors: bool,
}

/// Pack a pixi environment.
pub async fn pack(options: PackOptions) -> Result<()> {
    let lockfile_path = options
        .manifest_path
        .parent()
        .ok_or(anyhow!("could not get parent directory"))?
        .join("pixi.lock");

    let lockfile = LockFile::from_path(&lockfile_path).map_err(|e| {
        anyhow!(
            "could not read lockfile at {}: {}",
            lockfile_path.display(),
            e
        )
    })?;

    let client = reqwest_client_from_auth_storage(options.auth_file)
        .map_err(|e| anyhow!("could not create reqwest client from auth storage: {e}"))?;

    let env = lockfile.environment(&options.environment).ok_or(anyhow!(
        "environment not found in lockfile: {}",
        options.environment
    ))?;

    let packages = env.packages(options.platform).ok_or(anyhow!(
        "platform not found in lockfile: {}",
        options.platform.as_str()
    ))?;

    let output_folder =
        tempfile::tempdir().map_err(|e| anyhow!("could not create temporary directory: {}", e))?;

    let channel_dir = output_folder.path().join(CHANNEL_DIRECTORY_NAME);

    let mut conda_packages_from_lockfile: Vec<CondaPackage> = Vec::new();

    for package in packages {
        match package {
            Package::Conda(p) => conda_packages_from_lockfile.push(p),
            Package::Pypi(_) => {
                if options.ignore_pypi_errors {
                    tracing::warn!(
                        "ignoring PyPI package since PyPI packages are not supported by pixi-pack"
                    );
                } else {
                    anyhow::bail!("PyPI packages are not supported in pixi-pack");
                }
            }
        }
    }

    // Download packages to temporary directory.
    tracing::info!(
        "Downloading {} packages...",
        conda_packages_from_lockfile.len()
    );
    eprintln!(
        "⏳ Downloading {} packages...",
        conda_packages_from_lockfile.len()
    );
    let bar = ProgressReporter::new(conda_packages_from_lockfile.len() as u64);
    stream::iter(conda_packages_from_lockfile.iter())
        .map(Ok)
        .try_for_each_concurrent(50, |package| async {
            download_package(&client, package, &channel_dir).await?;
            bar.pb.inc(1);
            Ok(())
        })
        .await
        .map_err(|e: anyhow::Error| anyhow!("could not download package: {}", e))?;
    bar.pb.finish_and_clear();

    let mut conda_packages: Vec<(String, PackageRecord)> = Vec::new();

    for package in conda_packages_from_lockfile {
        let filename = package
            .file_name()
            .ok_or(anyhow!("could not get file name"))?
            .to_string();
        conda_packages.push((filename, package.package_record().clone()));
    }

    let injected_packages: Vec<(PathBuf, ArchiveType)> = options
        .injected_packages
        .iter()
        .filter_map(|e| {
            ArchiveType::split_str(e.as_path().to_string_lossy().as_ref())
                .map(|(p, t)| (PathBuf::from(format!("{}{}", p, t.extension())), t))
        })
        .collect();

    tracing::info!("Injecting {} packages", injected_packages.len());
    for (path, archive_type) in injected_packages.iter() {
        // step 1: Derive PackageRecord from index.json inside the package
        let package_record = match archive_type {
            ArchiveType::TarBz2 => package_record_from_tar_bz2(path),
            ArchiveType::Conda => package_record_from_conda(path),
        }?;

        // step 2: Copy file into channel dir
        let subdir = &package_record.subdir;
        let filename = path
            .file_name()
            .ok_or(anyhow!("could not get file name"))?
            .to_str()
            .ok_or(anyhow!("could not convert filename to string"))?
            .to_string();

        fs::copy(&path, channel_dir.join(subdir).join(&filename))
            .await
            .map_err(|e| anyhow!("could not copy file to channel directory: {}", e))?;

        conda_packages.push((filename, package_record));
    }

    // In case we injected packages, we need to validate that these packages are solvable with the
    // environment (i.e., that each packages dependencies and run constraints are still satisfied).
    if !injected_packages.is_empty() {
        PackageRecord::validate(conda_packages.iter().map(|(_, p)| p.clone()).collect())?;
    }

    // Create `repodata.json` files.
    tracing::info!("Creating repodata.json files");
    create_repodata_files(conda_packages.iter(), &channel_dir).await?;

    // Add pixi-pack.json containing metadata.
    tracing::info!("Creating pixi-pack.json file");
    let metadata_path = output_folder.path().join(PIXI_PACK_METADATA_PATH);
    let metadata = serde_json::to_string_pretty(&options.metadata)?;
    fs::write(metadata_path, metadata.as_bytes()).await?;

    // Create environment file.
    tracing::info!("Creating environment.yml file");
    create_environment_file(output_folder.path(), conda_packages.iter().map(|(_, p)| p)).await?;

    // Adjusting all timestamps of directories and files (excl. conda packages).
    for entry in WalkDir::new(output_folder.path())
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        match entry.path().extension().and_then(|e| e.to_str()) {
            Some("bz2") | Some("conda") => continue,
            _ => {
                set_default_file_times(entry.path()).map_err(|e| {
                    anyhow!(
                        "could not set default file times for path {}: {}",
                        entry.path().display(),
                        e
                    )
                })?;
            }
        }
    }

    // Pack = archive the contents.
    tracing::info!("Creating archive at {}", options.output_file.display());
    archive_directory(output_folder.path(), &options.output_file)
        .await
        .map_err(|e| anyhow!("could not archive directory: {}", e))?;

    let output_size = HumanBytes(get_size(&options.output_file)?).to_string();
    tracing::info!(
        "Created pack at {} with size {}.",
        options.output_file.display(),
        output_size
    );
    eprintln!(
        "📦 Created pack at {} with size {}.",
        options.output_file.display(),
        output_size
    );

    Ok(())
}

/// Get the authentication storage from the given auth file path.
fn get_auth_store(auth_file: Option<PathBuf>) -> Result<AuthenticationStorage> {
    match auth_file {
        Some(auth_file) => Ok(AuthenticationStorage::from_file(&auth_file)?),
        None => Ok(rattler_networking::AuthenticationStorage::default()),
    }
}

/// Create a reqwest client (optionally including authentication middleware).
fn reqwest_client_from_auth_storage(auth_file: Option<PathBuf>) -> Result<ClientWithMiddleware> {
    let auth_storage = get_auth_store(auth_file)?;

    let timeout = 5 * 60;
    let client = reqwest_middleware::ClientBuilder::new(
        reqwest::Client::builder()
            .no_gzip()
            .pool_max_idle_per_host(20)
            .user_agent("pixi-pack")
            .timeout(std::time::Duration::from_secs(timeout))
            .build()
            .map_err(|e| anyhow!("could not create download client: {}", e))?,
    )
    .with_arc(Arc::new(AuthenticationMiddleware::new(auth_storage)))
    .build();
    Ok(client)
}

/// Download a conda package to a given output directory.
async fn download_package(
    client: &ClientWithMiddleware,
    package: &CondaPackage,
    output_dir: &Path,
) -> Result<()> {
    let output_dir = output_dir.join(&package.package_record().subdir);
    create_dir_all(&output_dir)
        .await
        .map_err(|e| anyhow!("could not create download directory: {}", e))?;

    let file_name = package
        .file_name()
        .ok_or(anyhow!("could not get file name"))?;
    let package_url = package.url();

    match package_url.scheme() {
        "file" => {
            // Handle file:// URLs
            let local_path = package_url
                .to_file_path()
                .map_err(|_| anyhow!("Invalid file URL: {}", package_url))?;
            tracing::debug!("Resolved local path: {}", local_path.display());

            // Check file existence
            if !local_path.exists() {
                return Err(anyhow!(
                    "File does not exist at path: {}",
                    local_path.display()
                ));
            }

            // Check file accessibility
            if let Err(e) = tokio::fs::File::open(&local_path).await {
                tracing::error!("Failed to open file {}: {:?}", local_path.display(), e);
                return Err(anyhow!(
                    "Could not access file at path {}: {:?}",
                    local_path.display(),
                    e
                ));
            }

            // Ensure target directory exists
            let target_path = output_dir.join(file_name);
            create_dir_all(target_path.parent().unwrap_or_else(|| Path::new(".")))
                .await
                .map_err(|e| anyhow!("Could not create target directory: {}", e))?;

            // Copy file
            fs::copy(&local_path, &target_path)
                .await
                .map_err(|e| anyhow!(
                    "Could not copy file from {} to {}: {}",
                    local_path.display(),
                    target_path.display(),
                    e
                ))?;
        }
        "http" | "https" => {
            // Handle HTTP or HTTPS URLs
            let mut dest = File::create(output_dir.join(file_name)).await?;

            tracing::debug!("Fetching package {}", package.url());
            let mut response = client.get(package_url.to_string()).send().await?;
            if response.status().is_client_error() {
                return Err(anyhow!(
                    "failed to download {}: {}",
                    package.url().to_string(),
                    response.text().await?
                ));
            }

            while let Some(chunk) = response.chunk().await? {
                dest.write_all(&chunk).await?;
            }

            // Adjust file metadata (timestamps).
            let package_timestamp = package
                .package_record()
                .timestamp
                .map(|ts| ts.into())
                .unwrap_or_else(|| {
                    tracing::error!(
                        "could not get timestamp of {:?}, using default",
                        package.file_name()
                    );
                    std::time::SystemTime::UNIX_EPOCH
                });
            let file_times = FileTimes::new()
                .set_modified(package_timestamp)
                .set_accessed(package_timestamp);

            // Make sure to write all data and metadata to disk before modifying timestamp.
            dest.sync_all().await?;
            let dest_file = dest
                .try_into_std()
                .map_err(|e| anyhow!("could not read standard file: {:?}", e))?;
            dest_file.set_times(file_times)?;
        }
        other => {
            // Handle unsupported URL schemes
            return Err(anyhow!("unsupported URL scheme: {}", other));
        }
    }

    Ok(())
}

/// Archive a directory into a tarball.
async fn archive_directory(input_dir: &Path, archive_target: &Path) -> Result<()> {
    let outfile = fs::File::create(archive_target).await.map_err(|e| {
        anyhow!(
            "could not create archive file at {}: {}",
            archive_target.display(),
            e
        )
    })?;

    let writer = tokio::io::BufWriter::new(outfile);
    let mut archive = Builder::new(writer);

    archive
        .append_dir_all(".", input_dir)
        .await
        .map_err(|e| anyhow!("could not append directory to archive: {}", e))?;

    let mut compressor = archive
        .into_inner()
        .await
        .map_err(|e| anyhow!("could not finish writing archive: {}", e))?;

    compressor
        .shutdown()
        .await
        .map_err(|e| anyhow!("could not flush output: {}", e))?;

    Ok(())
}

/// Create an `environment.yml` file from the given packages.
async fn create_environment_file(
    destination: &Path,
    packages: impl IntoIterator<Item = &PackageRecord>,
) -> Result<()> {
    let environment_path = destination.join("environment.yml");

    let mut environment = String::new();

    environment.push_str("channels:\n");
    environment.push_str(&format!("  - ./{CHANNEL_DIRECTORY_NAME}\n",));
    environment.push_str("  - nodefaults\n");
    environment.push_str("dependencies:\n");

    for package in packages {
        let match_spec_str = format!(
            "{}={}={}",
            package.name.as_normalized(),
            package.version,
            package.build,
        );

        environment.push_str(&format!("  - {}\n", match_spec_str));
    }

    fs::write(environment_path.as_path(), environment)
        .await
        .map_err(|e| anyhow!("Could not write environment file: {}", e))?;

    Ok(())
}

/// Create `repodata.json` files for the given packages.
async fn create_repodata_files(
    packages: impl Iterator<Item = &(String, PackageRecord)>,
    channel_dir: &Path,
) -> Result<()> {
    let mut packages_per_subdir = HashMap::new();

    for (filename, p) in packages {
        let subdir = &p.subdir;

        let packages = packages_per_subdir
            .entry(subdir)
            .or_insert_with(HashMap::new);
        packages.insert(filename, p);
    }

    for (subdir, packages) in packages_per_subdir {
        let repodata_path = channel_dir.join(subdir).join("repodata.json");

        let conda_packages: FxHashMap<_, _> = packages
            .into_iter()
            .map(|(filename, p)| (filename.to_string(), p.clone()))
            .collect();

        let repodata = RepoData {
            info: Some(ChannelInfo {
                subdir: subdir.clone(),
                base_url: None,
            }),
            packages: HashMap::default(),
            conda_packages,
            removed: HashSet::default(),
            version: Some(2),
        };

        let repodata_json = serde_json::to_string_pretty(&repodata)
            .map_err(|e| anyhow!("could not serialize repodata: {}", e))?;
        fs::write(repodata_path.as_path(), repodata_json)
            .map_err(|e| anyhow!("could not write repodata: {}", e))
            .await?;
    }

    Ok(())
}
