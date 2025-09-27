use std::fs::{create_dir_all, write};

use which::which;

pub fn create_systemd_unit(server: &Option<String>) -> anyhow::Result<()> {
    let out_str_static = r#"[Unit]
Description=A music player for the Lyrion Music Server
After=network-online.target sound.target

[Service]
Type=simple
ExecStart={path}{server}
Restart=on-failure

[Install]
WantedBy=default.target
"#;

    let mut out_str = if let Some(server) = server {
        out_str_static.replace("{server}", &format!(" --server {}", server))
    } else {
        out_str_static.replace("{server}", "")
    };

    let path = which("vibe")?;
    out_str = out_str.replace("{path}", &path.to_string_lossy());

    let config_dir = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not find config directory"))?
        .join("systemd/user");

    create_dir_all(&config_dir)?;

    let unit_file = config_dir.join("vibe.service");
    write(&unit_file, out_str)?;

    println!("Successfully installed systemd service to: {}", unit_file.to_string_lossy());
	println!("To enable and start the service, run:");
	println!("  systemctl --user daemon-reload");
	println!("  systemctl --user enable --now vibe.service");

    Ok(())
}
