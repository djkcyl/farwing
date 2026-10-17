//! Mutable module preferences belong to the user, never the installed DLL.
use super::*;

pub fn path(id: &str) -> Result<PathBuf> {
    ensure!(super::valid_id(id), "插件 ID 无效");

    let base = crate::platform::paths::require_local_app_data()?.join("OpenUUYC");

    ensure!(base.is_absolute(), "插件配置目录必须为绝对路径");
    Ok(base.join("plugin-settings").join(format!("{id}.json")))
}
pub fn read_required(path: &Path) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    ensure!(file.metadata()?.len() <= 65536, "插件配置过大");
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 65536, "插件配置过大");
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
    ensure!(config.is_object(), "模块配置必须为对象");
    let bytes = serde_json::to_vec_pretty(config)?;
    ensure!(bytes.len() <= 65536, "插件配置过大");
    ensure!(&read(path)? == expected, "配置已被其他程序修改，请重新打开");
    std::fs::create_dir_all(path.parent().context("配置目录无效")?)?;
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
            ensure!(&read(path)? == expected, "配置已被其他程序修改，请重新打开");
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
            serde_json::from_slice(&bytes).context("模块配置格式无效")?;
        let values = config.as_object().context("模块配置必须为对象")?;
        manifest
            .config
            .as_object_mut()
            .context("插件默认配置无效")?
            .extend(values.clone());
    }
    Ok(())
}
