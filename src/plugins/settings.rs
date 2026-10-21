//! Mutable module preferences belong to the user, never the installed DLL.
use super::*;
use crate::i18n::t;

pub fn path(id: &str) -> Result<PathBuf> {
    ensure!(super::valid_id(id), "{}", t().features.plugin_id_invalid);

    let base = crate::product::local_data_dir().context("LOCALAPPDATA unavailable")?;
    Ok(base.join("plugin-settings").join(format!("{id}.json")))
}
pub fn read_required(path: &Path) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    ensure!(
        file.metadata()?.len() <= 65536,
        "{}",
        t().features.plugin_settings_too_large
    );
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 65536,
        "{}",
        t().features.plugin_settings_too_large
    );
    Ok(bytes)
}
pub fn read(path: &Path) -> Result<Option<Vec<u8>>> {
    match read_required(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}
pub fn write(path: &Path, expected: &Option<Vec<u8>>, config: &serde_json::Value) -> Result<()> {
    ensure!(
        config.is_object(),
        "{}",
        t().features.plugin_settings_not_object
    );
    let bytes = serde_json::to_vec_pretty(config)?;
    ensure!(
        bytes.len() <= 65536,
        "{}",
        t().features.plugin_settings_too_large
    );
    ensure!(
        &read(path)? == expected,
        "{}",
        t().features.plugin_settings_changed
    );
    std::fs::create_dir_all(
        path.parent()
            .context(t().features.plugin_settings_folder_invalid)?,
    )?;
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        if expected.is_none() {
            // Publish a first configuration without replacing a concurrent writer.
            std::fs::hard_link(&temporary, path)?;
            std::fs::remove_file(&temporary)?;
        } else {
            ensure!(
                &read(path)? == expected,
                "{}",
                t().features.plugin_settings_changed
            );
            std::fs::rename(&temporary, path)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}
pub fn apply(manifest: &mut Manifest) -> Result<()> {
    if !manifest.nodes.is_empty() {
        return Ok(());
    }
    let path = path(&manifest.id)?;
    if let Some(bytes) = read(&path)? {
        let config: serde_json::Value =
            serde_json::from_slice(&bytes).context(t().features.plugin_settings_format_invalid)?;
        let values = config
            .as_object()
            .context(t().features.plugin_settings_not_object)?;
        manifest
            .config
            .as_object_mut()
            .context(t().features.plugin_default_settings_invalid)?
            .extend(values.clone());
    }
    Ok(())
}
