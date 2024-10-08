use super::mod_platform::ModPlatform;
use super::plugin_format::PluginFormat;
use super::plugins::SelectedPlugins;
use super::utils::{get_plugin_bundle_name, get_plugin_folder, get_plugin_path};
use super::zip_service::ZipService;
use super::Error;
use crate::mod_plugin_controller;
use futures::future::try_join_all;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use tauri::utils::platform::Target;

pub fn create_plugin_folders_on_mac_os(
    plugins: &SelectedPlugins,
    vst3_folder: &Option<String>,
    clap_folder: &Option<String>,
) -> Result<(), Error> {
    if Target::current() != Target::MacOS || (plugins.vst3.is_empty() && plugins.clap.is_empty()) {
        return Ok(());
    }

    let vst3_plugin_paths =
        concatenate_plugin_paths(&plugins.vst3, PluginFormat::VST3, vst3_folder)?;
    let clap_plugin_paths =
        concatenate_plugin_paths(&plugins.clap, PluginFormat::CLAP, clap_folder)?;
    let plugin_paths = format!("{vst3_plugin_paths} {clap_plugin_paths}");

    let username_cmd = Command::new("id").arg("-un").output()?;
    if username_cmd.status.success() {
        let username = String::from_utf8_lossy(&username_cmd.stdout)
            .trim()
            .to_string();

        let create_dir_script = format!(
            r#"do shell script "mkdir -p {1} && chmod 755 {1} && chown -R {0}: {1}" with administrator privileges"#,
            username,
            plugin_paths.trim()
        );
        let create_dir_cmd = Command::new("osascript")
            .arg("-e")
            .arg(create_dir_script)
            .output()?;

        if create_dir_cmd.status.success() {
            return Ok(());
        } else {
            return Err(Error::CreateDirectoryError(
                String::from_utf8_lossy(&create_dir_cmd.stderr).to_string(),
            ));
        }
    } else {
        return Err(Error::CreateDirectoryError(
            String::from_utf8_lossy(&username_cmd.stderr).to_string(),
        ));
    }
}

pub fn remove_plugin_folders_on_mac_os(
    plugins: &Vec<String>,
    plugin_format: PluginFormat,
    folder: &Option<String>,
) -> Result<(), Error> {
    if Target::current() != Target::MacOS {
        return Ok(());
    }

    let plugin_paths = concatenate_plugin_paths(&plugins, plugin_format, folder)?;

    let remove_dir_script = format!(
        r#"do shell script "rm -rf {}" with administrator privileges"#,
        plugin_paths.trim()
    );
    let remove_dir_cmd = Command::new("osascript")
        .arg("-e")
        .arg(remove_dir_script)
        .output()?;

    if remove_dir_cmd.status.success() {
        return Ok(());
    } else {
        return Err(Error::CreateDirectoryError(
            String::from_utf8_lossy(&remove_dir_cmd.stderr).to_string(),
        ));
    }
}

pub async fn create_vst_or_clap_plugins(
    plugins: &Vec<String>,
    target_plugin_format: PluginFormat,
    folder: &Option<String>,
) -> Result<(), Error> {
    if plugins.is_empty() {
        return Ok(());
    }

    let plugin_folder = if let Some(folder) = folder {
        PathBuf::from(folder)
    } else {
        get_plugin_folder(&target_plugin_format)?
    };

    let futures: Vec<_> = plugins
        .iter()
        .map(|plugin| {
            let plugin_folder = plugin_folder.clone();
            let plugin_format = target_plugin_format.clone();

            async move {
                create_plugin(&plugin_folder, plugin.as_str(), plugin_format).await?;
                Ok::<(), Error>(())
            }
        })
        .collect();

    try_join_all(futures).await?;

    Ok(())
}

pub async fn create_mod_plugins(plugins: Vec<String>, platform: &String) -> Result<(), Error> {
    if plugins.is_empty() {
        return Ok(());
    }

    let futures: Vec<_> = plugins
        .iter()
        .map(|plugin| async move {
            create_mod_plugin(plugin.as_str(), platform).await?;
            Ok::<(), Error>(())
        })
        .collect();

    try_join_all(futures).await?;

    Ok(())
}

async fn create_mod_plugin(plugin_name: &str, mod_platform: &String) -> Result<(), Error> {
    let mod_platform = map_mod_platform(mod_platform);
    let zipfile_path = download_zip_file(plugin_name, mod_platform).await?;
    let bundle_name = get_plugin_bundle_name(plugin_name, &PluginFormat::ModAudio)?;
    let starts_with = match zipfile_path.with_extension("").file_name() {
        Some(folder) => Ok(PathBuf::from(folder).join(&bundle_name)),
        None => Err(Error::NoDownloadFile),
    }?;

    let unzip_result = ZipService::unzip_to_u8(&zipfile_path, &starts_with).map_err(Error::from);
    let copy_result = match unzip_result {
        Ok(files) => mod_plugin_controller::create_mod_plugins(files)
            .await
            .map_err(Error::from),
        Err(e) => Err(e),
    };
    match copy_result {
        Ok(_) => {
            fs::remove_file(&zipfile_path)?;
            return Ok(());
        }
        Err(e) => {
            fs::remove_file(&zipfile_path)?;
            return Err(e);
        }
    }
}

async fn create_plugin(
    plugin_folder: &PathBuf,
    plugin_name: &str,
    plugin_format: PluginFormat,
) -> Result<(), Error> {
    let bundle_name = get_plugin_bundle_name(plugin_name, &plugin_format)?;
    let plugin_path = get_plugin_path(plugin_folder, plugin_name, &plugin_format)?;
    let zipfile_path = download_zip_file(plugin_name, None).await?;
    let unzipped_folder = zipfile_path.with_extension("");

    let unzip_result = ZipService::unzip(&zipfile_path).map_err(Error::from);
    let copy_result = unzip_result.and_then(|_| {
        copy_dir_all(unzipped_folder.join(&bundle_name), &plugin_path).map_err(Error::from)
    });
    match copy_result {
        Ok(_) => {
            fs::remove_file(&zipfile_path)?;
            fs::remove_dir_all(unzipped_folder)?;
            return Ok(());
        }
        Err(e) => {
            fs::remove_file(&zipfile_path)?;
            fs::remove_dir_all(unzipped_folder)?;
            fs::remove_dir_all(&plugin_path)?;
            return Err(e);
        }
    }
}

async fn download_zip_file(
    plugin_name: &str,
    mod_platform: Option<ModPlatform>,
) -> Result<PathBuf, Error> {
    let download_file_name = get_download_file_name(plugin_name, mod_platform)?;
    let url = format!(
        "https://github.com/davemollen/{0}/releases/latest/download/{1}",
        plugin_name, download_file_name
    );
    let response = reqwest::get(url).await?;
    let zipfile_content = response.bytes().await?;
    let zipfile_path = std::env::temp_dir().join(&download_file_name);
    let mut zipfile = File::create(&zipfile_path)?;

    match {
        io::copy(&mut zipfile_content.as_ref(), &mut zipfile)?;
        Ok(())
    } {
        Ok(()) => Ok(zipfile_path),
        Err(e) => {
            fs::remove_file(&zipfile_path)?;
            Err(e)
        }
    }
}

fn get_download_file_name(
    plugin_name: &str,
    mod_platform: Option<ModPlatform>,
) -> Result<String, Error> {
    let os = match (Target::current(), mod_platform) {
        (Target::MacOS, None) => Ok("vst-and-clap-macos".to_string()),
        (Target::Windows, None) => Ok("vst-and-clap-windows".to_string()),
        (Target::Linux, None) => Ok("vst-and-clap-ubuntu".to_string()),
        (_, Some(mod_platform)) => match mod_platform {
            ModPlatform::Dwarf => Ok("moddwarf-new".to_string()),
            ModPlatform::Duo => Ok("modduo-new".to_string()),
            ModPlatform::DuoX => Ok("modduox-new".to_string()),
        },
        (_, None) => Err(Error::NoDownloadFile),
    }?;

    Ok(format!("{0}-{1}.zip", plugin_name, os))
}

fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> Result<(), Error> {
    fs::create_dir_all(&dst)?;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_all(entry.path(), dst.as_ref().join(entry.file_name()))?;
        } else {
            fs::copy(entry.path(), dst.as_ref().join(entry.file_name()))?;
        }
    }

    Ok(())
}

fn map_mod_platform(input: &String) -> Option<ModPlatform> {
    match input.as_str() {
        "Duo" => Some(ModPlatform::Duo),
        "DuoX" => Some(ModPlatform::DuoX),
        "Dwarf" => Some(ModPlatform::Dwarf),
        _ => None,
    }
}

fn concatenate_plugin_paths(
    plugins: &Vec<String>,
    plugin_format: PluginFormat,
    folder: &Option<String>,
) -> Result<String, Error> {
    let plugin_folder = if let Some(folder) = folder {
        PathBuf::from(folder)
    } else {
        get_plugin_folder(&plugin_format)?
    };

    if !plugins.is_empty() {
        let plugin_paths = plugins
            .iter()
            .map(|plugin| {
                let path = get_plugin_path(&plugin_folder, plugin, &plugin_format)?;
                Ok(path.to_str().unwrap_or_default().to_string())
            })
            .fold("".to_string(), |result, path: Result<String, Error>| {
                match path {
                    Err(_) => return result,
                    Ok(path) => {
                        return format!("{} {}", result, path).into();
                    }
                };
            });

        Ok(plugin_paths)
    } else {
        Ok("".to_string())
    }
}
