use std::{
    ffi::OsStr,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use derive_more::Display;

use futures::{channel::oneshot, StreamExt};
use humansize::{format_size, DECIMAL};
use once_cell::sync::Lazy;
use tempfile::TempDir;
use tokio::sync::mpsc;

use crate::{state::storage::Storage as uplink_storage, VIDEO_FILE_EXTENSIONS};
use crate::{warp_runner::Storage as warp_storage, DOC_EXTENSIONS};

use warp::{
    constellation::{
        directory::Directory,
        file::File,
        item::{FormatType, Item, ItemType},
        Progression,
    },
    error::Error,
    logging::tracing::log,
    sync::RwLock,
};

static DIRECTORIES_AVAILABLE_TO_BROWSE: Lazy<RwLock<Vec<Directory>>> =
    Lazy::new(|| RwLock::new(Vec::new()));

pub enum FileTransferStep {
    SizeNotAvailable(String),
    Start(String),
    DuplicateName(Option<String>),
    Upload(String),
    Thumbnail(Option<()>),
}

pub enum FileTransferProgress<T> {
    Error(warp::error::Error),
    Finished(T),
    Step(FileTransferStep),
}

#[derive(Display)]
pub enum ConstellationCmd {
    #[display(fmt = "GetItemsFromCurrentDirectory")]
    GetItemsFromCurrentDirectory {
        rsp: oneshot::Sender<Result<uplink_storage, warp::error::Error>>,
    },
    #[display(fmt = "CreateNewDirectory {{ directory_name: {directory_name} }} ")]
    CreateNewDirectory {
        directory_name: String,
        rsp: oneshot::Sender<Result<(), warp::error::Error>>,
    },
    #[display(fmt = "OpenDirectory {{ directory_name: {directory_name} }} ")]
    OpenDirectory {
        directory_name: String,
        rsp: oneshot::Sender<Result<uplink_storage, warp::error::Error>>,
    },
    #[display(fmt = "BackToPreviousDirectory")]
    BackToPreviousDirectory {
        directory: Directory,
        rsp: oneshot::Sender<Result<uplink_storage, warp::error::Error>>,
    },
    #[display(fmt = "UploadFiles {{ files_path: {files_path:?} }} ")]
    UploadFiles {
        files_path: Vec<PathBuf>,
        rsp: mpsc::UnboundedSender<FileTransferProgress<uplink_storage>>,
    },
    #[display(fmt = "RenameItems {{ old_name: {old_name}, new_name: {new_name} }} ")]
    RenameItem {
        old_name: String,
        new_name: String,
        rsp: oneshot::Sender<Result<uplink_storage, warp::error::Error>>,
    },
    #[display(
        fmt = "DownloadItems {{ file_name: {file_name:?}, local_path_to_save_file: {local_path_to_save_file:?} }} "
    )]
    DownloadFile {
        file_name: String,
        local_path_to_save_file: PathBuf,
        rsp: oneshot::Sender<Result<(), warp::error::Error>>,
    },
    #[display(fmt = "DeleteItems {{ item: {item:?} }} ")]
    DeleteItems {
        item: Item,
        rsp: oneshot::Sender<Result<uplink_storage, warp::error::Error>>,
    },
    #[display(fmt = "GetStorageSize")]
    GetStorageSize {
        rsp: oneshot::Sender<Result<(usize, usize), warp::error::Error>>,
    },
}

pub async fn handle_constellation_cmd(cmd: ConstellationCmd, warp_storage: &mut warp_storage) {
    match cmd {
        ConstellationCmd::GetItemsFromCurrentDirectory { rsp } => {
            let r = get_items_from_current_directory(warp_storage);
            let _ = rsp.send(r);
        }
        ConstellationCmd::CreateNewDirectory {
            directory_name,
            rsp,
        } => {
            let r = create_new_directory(&directory_name, warp_storage).await;
            let _ = rsp.send(r);
        }
        ConstellationCmd::OpenDirectory {
            directory_name,
            rsp,
        } => {
            let r = open_new_directory(warp_storage, &directory_name);
            let _ = rsp.send(r);
        }
        ConstellationCmd::BackToPreviousDirectory { directory, rsp } => {
            let r = go_back_to_previous_directory(warp_storage, directory);
            let _ = rsp.send(r);
        }
        ConstellationCmd::UploadFiles { files_path, rsp } => {
            upload_files(warp_storage, files_path, rsp).await;
        }
        ConstellationCmd::DownloadFile {
            file_name,
            local_path_to_save_file,
            rsp,
        } => {
            let r = download_file(warp_storage, file_name, local_path_to_save_file).await;
            let _ = rsp.send(r);
        }
        ConstellationCmd::RenameItem {
            old_name,
            new_name,
            rsp,
        } => {
            let r = rename_item(old_name, new_name, warp_storage).await;
            let _ = rsp.send(r);
        }
        ConstellationCmd::DeleteItems { item, rsp } => {
            let r = delete_items(warp_storage, item).await;
            let _ = rsp.send(r);
        }
        ConstellationCmd::GetStorageSize { rsp } => {
            let r = get_storage_size(warp_storage);
            let _ = rsp.send(r);
        }
    }
}

async fn delete_items(
    warp_storage: &mut warp_storage,
    item: Item,
) -> Result<uplink_storage, Error> {
    // If is file, just a small function solve it
    if item.is_file() {
        let file_name = item.name();
        match warp_storage.remove(&file_name, false).await {
            Ok(_) => log::info!("File deleted: {:?}", file_name),
            Err(error) => log::error!("Error to delete file {:?}, {:?}", file_name, error),
        };
        return get_items_from_current_directory(warp_storage);
    };
    // Code keeps here just if item is a directory
    let first_dir = warp_storage.current_directory()?;
    let mut current_dirs_opened = get_directories_opened();
    current_dirs_opened.push(first_dir.clone());

    let mut dirs: Vec<Directory> = current_dirs_opened;

    match warp_storage.select(&item.name()) {
        Ok(_) => log::debug!("Selected new dir: {:?}.", item.name()),
        Err(error) => {
            log::error!("Error to select new dir: {:?}.", error);
            return Err(error);
        }
    };
    dirs.push(warp_storage.current_directory()?);

    while let Some(last_dir) = dirs.clone().last() {
        if last_dir.id() == first_dir.id() {
            set_new_directory_opened(dirs.as_mut(), last_dir.clone());
            break;
        };

        let dir_items = last_dir.get_items();

        let is_there_file_yet = last_dir
            .get_items()
            .iter()
            .any(|f| f.item_type() == ItemType::FileItem);

        let is_there_directory_yet = last_dir
            .get_items()
            .iter()
            .any(|f| f.item_type() == ItemType::DirectoryItem);

        // If there is a sub directory yet
        // Select it and keep loop
        if is_there_directory_yet {
            for item in dir_items {
                if item.is_directory() {
                    match warp_storage.select(&item.name()) {
                        Ok(_) => log::debug!("Selected new dir: {:?}.", item.name()),
                        Err(error) => {
                            log::error!("Error to select new dir: {:?}.", error);
                            return Err(error);
                        }
                    };
                    dirs.push(warp_storage.current_directory()?);
                    break;
                }
            }
            continue;
        };

        // No more files, it pop current dir on dirs variable
        // And remove current dir.
        //
        // After it, back to previous dir and keep loop verifying other files and sub dirs.
        if !is_there_file_yet {
            dirs.pop();
            if let Some(previous_dir) = dirs.last() {
                previous_dir.remove_item(&last_dir.name())?;
                log::info!("Directory {:?} was removed.", &last_dir.name());
            };

            match warp_storage.go_back() {
                Ok(_) => {
                    log::debug!(
                        "Selected new dir: {:?}",
                        warp_storage.current_directory()?.name()
                    );
                }
                Err(error) => {
                    log::error!("Error on go back a directory: {error}");
                    return Err(error);
                }
            };
            continue;
        }

        // If code arrives here, just run into files inside that dir and delete all of them.
        for file in last_dir.get_items().iter().filter(|f| f.is_file()) {
            match warp_storage.remove(&file.name(), false).await {
                Ok(_) => log::info!(
                    "File deleted: {:?}, on directory: {:?}.",
                    file.name(),
                    warp_storage.current_directory()?.name()
                ),
                Err(error) => {
                    log::error!("Error to delete this file: {:?}, {:?}", file.name(), error)
                }
            };
        }
    }
    get_items_from_current_directory(warp_storage)
}

async fn rename_item(
    old_name: String,
    new_name: String,
    warp_storage: &mut warp_storage,
) -> Result<uplink_storage, Error> {
    if let Err(error) = warp_storage.rename(&old_name, &new_name).await {
        log::error!("Failed to rename item: {error}");
    }

    get_items_from_current_directory(warp_storage)
}

fn get_storage_size(warp_storage: &warp_storage) -> Result<(usize, usize), Error> {
    Ok((warp_storage.max_size(), warp_storage.current_size()))
}

async fn create_new_directory(
    folder_name: &str,
    warp_storage: &mut warp_storage,
) -> Result<(), Error> {
    warp_storage.create_directory(folder_name, true).await?;
    log::debug!("New directory created: {:?}", folder_name);
    Ok(())
}

fn get_items_from_current_directory(
    warp_storage: &mut warp_storage,
) -> Result<uplink_storage, Error> {
    let current_dir = warp_storage.current_directory()?;
    let mut current_dirs = get_directories_opened();
    set_new_directory_opened(current_dirs.as_mut(), current_dir.clone());

    let items = current_dir.get_items();

    let mut directories = items
        .iter()
        .filter_map(|item| item.get_directory().ok())
        .collect::<Vec<_>>();
    let mut files = items
        .iter()
        .filter_map(|item| item.get_file().ok())
        .collect::<Vec<_>>();

    directories.sort_by_key(|b| std::cmp::Reverse(b.modified()));
    files.sort_by_key(|b| std::cmp::Reverse(b.modified()));

    let uplink_storage = uplink_storage {
        initialized: true,
        current_dir,
        directories_opened: get_directories_opened(),
        directories,
        files,
    };
    log::info!("Get items from current directory worked!");
    Ok(uplink_storage)
}

fn get_directories_opened() -> Vec<Directory> {
    DIRECTORIES_AVAILABLE_TO_BROWSE.read().to_owned()
}

fn set_new_directory_opened(current_dir: &mut Vec<Directory>, new_dir: Directory) {
    if !current_dir.contains(&new_dir) {
        log::debug!("Updating directories opened to browse");
        current_dir.push(new_dir);
        *DIRECTORIES_AVAILABLE_TO_BROWSE.write() = current_dir.to_owned()
    }
}

fn open_new_directory(
    warp_storage: &mut warp_storage,
    folder_name: &str,
) -> Result<uplink_storage, Error> {
    let current_path = PathBuf::from(
        warp_storage
            .get_path()
            .join(folder_name)
            .to_string_lossy()
            .replace('\\', "/"),
    );

    warp_storage.set_path(current_path);

    log::info!(
        "Navigation to directory {:?} worked!",
        warp_storage.get_path()
    );
    get_items_from_current_directory(warp_storage)
}

fn go_back_to_previous_directory(
    warp_storage: &mut warp_storage,
    directory: Directory,
) -> Result<uplink_storage, Error> {
    let mut current_dirs = get_directories_opened();
    loop {
        let current_dir = warp_storage.current_directory()?;

        current_dirs.remove(current_dirs.len() - 1);

        if current_dir.id() == directory.id() {
            set_new_directory_opened(current_dirs.as_mut(), current_dir);
            break;
        }

        if let Err(error) = warp_storage.go_back() {
            log::error!("Error on go back a directory: {error}");
            return Err(error);
        };
    }
    log::info!("Navigation to directory {} worked!", directory.name());
    get_items_from_current_directory(warp_storage)
}

async fn upload_files(
    warp_storage: &mut warp_storage,
    files_path: Vec<PathBuf>,
    // todo: send FileTransferProgress::Step until done
    tx: mpsc::UnboundedSender<FileTransferProgress<uplink_storage>>,
) {
    let current_directory = match warp_storage.current_directory() {
        Ok(d) => d,
        Err(e) => {
            let _ = tx.send(FileTransferProgress::Error(e));
            return;
        }
    };

    let max_size_ipfs = warp_storage.max_size();
    let current_size_ipfs = warp_storage.current_size();

    for file_path in files_path {
        let mut filename = match file_path
            .file_name()
            .map(|file| file.to_string_lossy().to_string())
        {
            Some(file) => file,
            None => continue,
        };
        let local_path = Path::new(&file_path).to_string_lossy().to_string();

        let file_size = match tokio::fs::metadata(&local_path).await {
            Ok(metadata) => metadata.len() as usize,
            Err(e) => {
                log::error!("Not possible to get file size, error: {}", e);
                continue;
            }
        };

        if (current_size_ipfs + file_size) > max_size_ipfs {
            log::error!(
                "Not available space for this file, {}",
                file_path.to_string_lossy()
            );
            let file_name = match Path::new(&local_path).file_name() {
                Some(name) => name.to_str().unwrap_or(&local_path).to_string(),
                None => local_path.to_string(),
            };
            let _ = tx.send(FileTransferProgress::Step(
                FileTransferStep::SizeNotAvailable(file_name),
            ));
            continue;
        }

        let _ = tx.send(FileTransferProgress::Step(FileTransferStep::Start(
            filename.clone(),
        )));

        let original = filename.clone();
        let file = PathBuf::from(&original);
        let _ = tx.send(FileTransferProgress::Step(FileTransferStep::DuplicateName(
            None,
        )));
        filename = rename_if_duplicate(current_directory.clone(), filename.clone(), file);
        let _ = tx.send(FileTransferProgress::Step(FileTransferStep::DuplicateName(
            Some(filename.clone()),
        )));

        match warp_storage.put(&filename, &local_path).await {
            Ok(mut upload_progress) => {
                let mut previous_percentage: usize = 0;
                let mut upload_process_started = false;

                while let Some(upload_progress) = upload_progress.next().await {
                    match upload_progress {
                        Progression::CurrentProgress {
                            name,
                            current,
                            total,
                        } => {
                            if !upload_process_started {
                                upload_process_started = true;
                                log::info!("Starting upload for {name}");
                                log::info!("0% completed -> written 0 bytes")
                            };

                            if let Some(total) = total {
                                let current_percentage =
                                    (((current as f64) / (total as f64)) * 100.) as usize;
                                if previous_percentage != current_percentage {
                                    previous_percentage = current_percentage;
                                    let readable_current = format_size(current, DECIMAL);
                                    let percentage_number =
                                        ((current as f64) / (total as f64)) * 100.;

                                    let _ = tx.send(FileTransferProgress::Step(
                                        FileTransferStep::Upload(format!(
                                            "{}%",
                                            percentage_number as usize
                                        )),
                                    ));

                                    log::info!(
                                        "{}% completed -> written {readable_current}",
                                        percentage_number as usize
                                    )
                                }
                            }
                        }
                        Progression::ProgressComplete { name, total } => {
                            let total = total.unwrap_or_default();
                            let readable_total = format_size(total, DECIMAL);
                            let _ = tx.send(FileTransferProgress::Step(FileTransferStep::Upload(
                                readable_total.clone(),
                            )));
                            log::info!("{name} has been uploaded with {}", readable_total);
                        }
                        Progression::ProgressFailed {
                            name,
                            last_size,
                            error,
                        } => {
                            log::info!(
                                "{name} failed to upload at {} MB due to: {}",
                                last_size.unwrap_or_default(),
                                error.unwrap_or_default()
                            );
                        }
                    }
                }

                let video_formats = VIDEO_FILE_EXTENSIONS.to_vec();
                let doc_formats = DOC_EXTENSIONS.to_vec();

                let file_extension = std::path::Path::new(&filename)
                    .extension()
                    .and_then(OsStr::to_str)
                    .map(|s| format!(".{s}"))
                    .unwrap_or_default();

                if video_formats.iter().any(|f| f == &file_extension) {
                    match set_thumbnail_if_file_is_video(
                        warp_storage,
                        filename.clone(),
                        file_path.clone(),
                    )
                    .await
                    {
                        Ok(_) => {
                            log::info!("Video Thumbnail uploaded");
                            let _ = tx.send(FileTransferProgress::Step(
                                FileTransferStep::Thumbnail(Some(())),
                            ));
                        }
                        Err(error) => {
                            log::error!("Not possible to update thumbnail for video: {:?}", error);
                            let _ = tx.send(FileTransferProgress::Step(
                                FileTransferStep::Thumbnail(None),
                            ));
                        }
                    };
                }

                if doc_formats.iter().any(|f| f == &file_extension) {
                    match set_thumbnail_if_file_is_document(
                        warp_storage,
                        filename.clone(),
                        file_path.clone(),
                    )
                    .await
                    {
                        Ok(_) => {
                            log::info!("Document Thumbnail uploaded");
                            let _ = tx.send(FileTransferProgress::Step(
                                FileTransferStep::Thumbnail(Some(())),
                            ));
                        }
                        Err(error) => {
                            log::error!(
                                "Not possible to update thumbnail for document: {:?}",
                                error
                            );
                            let _ = tx.send(FileTransferProgress::Step(
                                FileTransferStep::Thumbnail(None),
                            ));
                        }
                    };
                }

                log::info!("{:?} file uploaded!", filename);
            }
            Err(error) => log::error!("Error when upload file: {:?}", error),
        }
    }
    let ret = match get_items_from_current_directory(warp_storage) {
        Ok(r) => FileTransferProgress::Finished(r),
        Err(e) => FileTransferProgress::Error(e),
    };
    let _ = tx.send(ret);
}

fn rename_if_duplicate(
    current_directory: Directory,
    filename: String,
    file_pathbuf: PathBuf,
) -> String {
    let mut count_index_for_duplicate_filename = 1;
    let mut new_file_name = filename.clone();
    let original = filename;
    loop {
        if !current_directory.has_item(&new_file_name) {
            break;
        }
        let file_extension = file_pathbuf
            .extension()
            .and_then(OsStr::to_str)
            .map(str::to_string);

        let file_stem = file_pathbuf
            .file_stem()
            .and_then(OsStr::to_str)
            .map(str::to_string);

        new_file_name = match (file_stem, file_extension) {
            (Some(file_stem), Some(file_extension)) => {
                format!("{file_stem} ({count_index_for_duplicate_filename}).{file_extension}")
            }
            _ => format!("{original} ({count_index_for_duplicate_filename})"),
        };
        log::info!("Duplicate name, changing file name to {}", new_file_name);
        count_index_for_duplicate_filename += 1;
    }
    new_file_name
}

async fn set_thumbnail_if_file_is_video(
    warp_storage: &warp_storage,
    filename_to_save: String,
    file_path: PathBuf,
) -> Result<(), Error> {
    let warp_storage = warp_storage.clone();
    tokio::task::spawn_blocking(move || {
        let item = warp_storage
            .current_directory()?
            .get_item(&filename_to_save)?;

        let file_stem = file_path
            .file_stem()
            .and_then(OsStr::to_str)
            .map(str::to_string)
            .unwrap_or_default();

        let temp_dir = TempDir::new()?;

        let temp_path = temp_dir.path().join(file_stem);

        let output = Command::new("ffmpeg")
            .args([
                "-i",
                &file_path.to_string_lossy(),
                "-vf",
                "select=eq(pict_type\\,I)",
                "-q:v",
                "2",
                "-f",
                "image2",
                "-update",
                "1",
                &temp_path.to_string_lossy(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        if let Some(mut child) = output.stdout {
            let mut contents = vec![];
            child.read_to_end(&mut contents)?;

            let image = std::fs::read(temp_path)?;

            item.set_thumbnail(&image);
            item.set_thumbnail_format(FormatType::Mime(
                "image/jpeg".parse().expect("Correct mime type"),
            ));
            Ok(())
        } else {
            log::warn!("Failed to save thumbnail from a video file");
            Err(Error::InvalidConversion)
        }
    })
    .await
    .map_err(anyhow::Error::from)?
}

async fn set_thumbnail_if_file_is_document(
    warp_storage: &warp_storage,
    filename_to_save: String,
    file_path: PathBuf,
) -> Result<(), Error> {
    let warp_storage = warp_storage.clone();
    tokio::task::spawn_blocking(move || {
        let item = warp_storage
            .current_directory()?
            .get_item(&filename_to_save)?;

        let file_stem = file_path
            .file_stem()
            .and_then(OsStr::to_str)
            .map(str::to_string)
            .unwrap_or_default();

        let temp_dir = TempDir::new()?;
        let temp_path = temp_dir.path().join(file_stem);

        let output = Command::new("pdftoppm")
            .args([
                "-jpeg",
                "-singlefile",
                "-scale-to",
                "500",
                "-r",
                "600",
                "-f",
                "1",
                "-l",
                "1",
                &file_path.to_string_lossy(),
                &temp_path.to_string_lossy(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        if output.stdout.is_some() {
            let path_2 = format!("{}.jpg", temp_path.to_string_lossy());
            std::thread::sleep(std::time::Duration::from_secs(1));
            let image = std::fs::read(path_2)?;
            item.set_thumbnail(&image);
            item.set_thumbnail_format(FormatType::Mime(
                "image/jpeg".parse().expect("Correct mime type"),
            ));
            Ok(())
        } else {
            log::warn!("Failed to save thumbnail from a document file");
            Err(Error::InvalidConversion)
        }
    })
    .await
    .map_err(anyhow::Error::from)?
}

async fn download_file(
    warp_storage: &warp_storage,
    file_name: String,
    local_path_to_save_file: PathBuf,
) -> Result<(), Error> {
    warp_storage
        .get(&file_name, &local_path_to_save_file.to_string_lossy())
        .await?;
    log::info!("{file_name} downloaded");
    Ok(())
}

pub fn thumbnail_to_base64(file: &File) -> String {
    let thumbnail = file.thumbnail();

    if thumbnail.is_empty() {
        return String::new();
    }

    let ty = file.thumbnail_format();
    let mime = match ty {
        FormatType::Mime(mime) => mime.to_string(),
        FormatType::Generic => "application/octet-stream".into(),
    };

    let prefix = format!("data:image/{mime};base64,");
    let base64_image = base64::encode(thumbnail);

    prefix + &base64_image
}
