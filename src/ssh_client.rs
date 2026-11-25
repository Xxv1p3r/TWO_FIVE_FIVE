//! Cliente SSH para modificar y diagnosticar parámetros dentro de máquinas virtuales.
//!
//! Usa el binario `ssh` del sistema con `SSH_ASKPASS` efímero (protegido con guardia RAII
//! que borra el script temporal inmediatamente al terminar) cuando se autentica por
//! contraseña, o llave privada `-i` cuando se indica `key_path`.
//! Por seguridad, restringe el destino a direcciones de laboratorio locales/privadas.

use std::fs;
use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static ASKPASS_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct SshTarget {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<String>,
    pub key_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SshResult {
    pub success: bool,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

struct AskpassGuard {
    path: PathBuf,
}

impl AskpassGuard {
    fn create(password: &str) -> Result<Self, String> {
        let seq = ASKPASS_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "tff_askpass_{}_{}.sh",
            std::process::id(),
            seq
        ));
        let script = "#!/bin/sh\nprintf '%s\\n' \"$TFF_SSH_PW\"\n";
        fs::write(&path, script)
            .map_err(|e| format!("No se pudo crear script askpass temporal: {}", e))?;
        let perms = fs::Permissions::from_mode(0o700);
        fs::set_permissions(&path, perms)
            .map_err(|e| format!("No se pudieron fijar permisos 0700 a askpass: {}", e))?;
        let _ = password; // La contraseña viaja vía env `TFF_SSH_PW`, nunca en disco
        Ok(Self { path })
    }
}

impl Drop for AskpassGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Verifica que el host SSH sea local o de red privada RFC1918 para evitar uso fuera del laboratorio.
pub fn validate_vm_host(host: &str) -> Result<(), String> {
    let h = host.trim();
    if h.is_empty() {
        return Err("El host SSH no puede estar vacío".to_string());
    }
    if h.eq_ignore_ascii_case("localhost") || h == "::1" || h == "127.0.0.1" {
        return Ok(());
    }
    if let Ok(ip) = h.parse::<Ipv4Addr>() {
        let oct = ip.octets();
        let is_loopback = oct[0] == 127;
        let is_rfc1918 = oct[0] == 10
            || (oct[0] == 172 && (16..=31).contains(&oct[1]))
            || (oct[0] == 192 && oct[1] == 168);
        if is_loopback || is_rfc1918 {
            return Ok(());
        }
        return Err(format!(
            "Host SSH '{}' bloqueado por seguridad: solo se permiten IPs de VM privadas o loopback",
            host
        ));
    }
    Err(format!(
        "Host SSH '{}' inválido: usa 127.0.0.1, localhost o una IP privada de VM (10.x / 172.16-31.x / 192.168.x)",
        host
    ))
}

pub fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

pub fn validate_hostname(name: &str) -> Result<&str, String> {
    let t = name.trim();
    if t.is_empty() || t.len() > 63 {
        return Err("Hostname debe tener entre 1 y 63 caracteres".to_string());
    }
    if t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.') {
        Ok(t)
    } else {
        Err(format!("Hostname inválido '{}' (solo letras, números, '-' y '.')", name))
    }
}

pub fn validate_sysctl_key(key: &str) -> Result<&str, String> {
    let t = key.trim();
    if t.is_empty() || !t.contains('.') {
        return Err(format!("Clave sysctl inválida '{}' (ej. net.ipv4.ip_forward)", key));
    }
    if t.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
        Ok(t)
    } else {
        Err(format!("Caracteres inválidos en clave sysctl '{}'", key))
    }
}

pub fn validate_ident<'a>(val: &'a str, kind: &str) -> Result<&'a str, String> {
    let t = val.trim();
    if t.is_empty() || t.len() > 96 {
        return Err(format!("{} inválido o vacío", kind));
    }
    if t.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | '+'))
    {
        Ok(t)
    } else {
        Err(format!("Caracteres no permitidos en {} '{}'", kind, val))
    }
}

fn truncate_output(raw: &[u8], limit: usize) -> String {
    let s = String::from_utf8_lossy(raw);
    if s.len() <= limit {
        s.to_string()
    } else {
        let mut end = limit;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}...\n[truncado]", &s[..end])
    }
}

/// Ejecuta un comando remoto dentro de la VM mediante OpenSSH con timeout real.
pub fn run_ssh_command(
    target: &SshTarget,
    remote_cmd: &str,
    timeout_secs: u64,
) -> Result<SshResult, String> {
    validate_vm_host(&target.host)?;
    let user = validate_ident(&target.user, "usuario SSH")?;
    if target.port == 0 {
        return Err("El puerto SSH debe ser > 0".to_string());
    }

    let mut cmd = Command::new("ssh");
    cmd.arg("-p")
        .arg(target.port.to_string())
        .arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-o")
        .arg("UserKnownHostsFile=/dev/null")
        .arg("-o")
        .arg("ConnectTimeout=8")
        .arg("-o")
        .arg("ServerAliveInterval=5")
        .arg("-o")
        .arg("ServerAliveCountMax=2")
        .arg("-o")
        .arg("LogLevel=ERROR");

    let mut _guard: Option<AskpassGuard> = None;
    if let Some(ref key_path) = target.key_path {
        if !key_path.trim().is_empty() {
            cmd.arg("-i")
                .arg(key_path.trim())
                .arg("-o")
                .arg("BatchMode=yes");
        }
    } else if let Some(ref pw) = target.password {
        let guard = AskpassGuard::create(pw)?;
        cmd.arg("-o").arg("PreferredAuthentications=password,keyboard-interactive");
        cmd.env("SSH_ASKPASS", &guard.path);
        cmd.env("SSH_ASKPASS_REQUIRE", "force");
        cmd.env("TFF_SSH_PW", pw);
        cmd.env("DISPLAY", "none");
        _guard = Some(guard);
    } else {
        cmd.arg("-o").arg("BatchMode=yes");
    }

    cmd.arg(format!("{}@{}", user, target.host.trim()))
        .arg(remote_cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("No se pudo lanzar cliente ssh: {}", e))?;

    let deadline = Instant::now() + Duration::from_secs(timeout_secs.max(5));
    let timed_out = loop {
        match child.try_wait() {
            Ok(Some(_)) => break false,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    break true;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("Error esperando proceso ssh: {}", e)),
        }
    };

    let output = child
        .wait_with_output()
        .map_err(|e| format!("Error recogiendo salida ssh: {}", e))?;
    if timed_out {
        return Err(format!(
            "Timeout SSH tras {}s ejecutando comando en {}:{}",
            timeout_secs, target.host, target.port
        ));
    }

    let exit_code = output.status.code().unwrap_or(-1);
    Ok(SshResult {
        success: output.status.success(),
        exit_code,
        stdout: truncate_output(&output.stdout, 8192),
        stderr: truncate_output(&output.stderr, 4096),
    })
}

fn privileged_prefix(user: &str) -> &'static str {
    if user.trim() == "root" {
        ""
    } else {
        "sudo -n "
    }
}

pub fn get_vm_info(target: &SshTarget) -> Result<SshResult, String> {
    let cmd = "echo \"HOSTNAME=$(hostname)\"; echo \"KERNEL=$(uname -srm)\"; \
               echo '--- IPS ---'; ip -4 -br addr 2>/dev/null || ifconfig -a 2>/dev/null; \
               echo '--- MEM ---'; free -m 2>/dev/null; \
               echo '--- DISK ---'; df -h / 2>/dev/null; \
               echo '--- UPTIME ---'; uptime 2>/dev/null";
    run_ssh_command(target, cmd, 15)
}

pub fn set_hostname(target: &SshTarget, new_hostname: &str) -> Result<SshResult, String> {
    let h = validate_hostname(new_hostname)?;
    let q = shell_single_quote(h);
    let p = privileged_prefix(&target.user);
    let cmd = format!(
        "({p}hostnamectl set-hostname {q} 2>/dev/null || ({p}sh -c \"printf '%s\\n' {q} > /etc/hostname\" && {p}hostname {q})) && echo HOSTNAME=$(hostname)",
        p = p,
        q = q
    );
    run_ssh_command(target, &cmd, 15)
}

pub fn set_sysctl(
    target: &SshTarget,
    key: &str,
    value: &str,
    persist: bool,
) -> Result<SshResult, String> {
    let k = validate_sysctl_key(key)?;
    let v = value.trim();
    if v.is_empty() || v.contains(['\n', '\r', ';', '&', '|']) {
        return Err("Valor sysctl inválido".to_string());
    }
    let kv = shell_single_quote(&format!("{}={}", k, v));
    let p = privileged_prefix(&target.user);
    let cmd = if persist {
        format!(
            "{p}sysctl -w {kv} && {p}sh -c \"mkdir -p /etc/sysctl.d && grep -v '^{k}=' /etc/sysctl.d/99-two55.conf 2>/dev/null > /tmp/.tff_sc || true; printf '%s\\n' {kv} >> /tmp/.tff_sc && mv /tmp/.tff_sc /etc/sysctl.d/99-two55.conf\"",
            p = p,
            k = k,
            kv = kv
        )
    } else {
        format!("{p}sysctl -w {kv}", p = p, kv = kv)
    };
    run_ssh_command(target, &cmd, 15)
}

pub fn configure_guest_network(
    target: &SshTarget,
    iface: &str,
    mode: &str,
    ip_cidr: Option<&str>,
    gateway: Option<&str>,
    dns: Option<&str>,
) -> Result<SshResult, String> {
    let dev = validate_ident(iface, "interfaz de red")?;
    let p = privileged_prefix(&target.user);
    let qdev = shell_single_quote(dev);
    let cmd = match mode.trim().to_ascii_lowercase().as_str() {
        "dhcp" => format!(
            "({p}nmcli dev set {d} managed yes 2>/dev/null && {p}nmcli dev reapply {d} 2>/dev/null) || {p}dhclient -r {d} 2>/dev/null; {p}dhclient {d} 2>/dev/null || {p}udhcpc -n -q -i {d}",
            p = p,
            d = qdev
        ),
        "static" => {
            let cidr = ip_cidr.ok_or_else(|| "Modo static requiere ip_cidr (ej. 10.0.2.20/24)".to_string())?;
            crate::network::parse_cidr(cidr)?;
            let qcidr = shell_single_quote(cidr.trim());
            let mut s = format!(
                "{p}ip addr flush dev {d} && {p}ip addr add {c} dev {d} && {p}ip link set {d} up",
                p = p,
                d = qdev,
                c = qcidr
            );
            if let Some(gw) = gateway {
                let qgw = shell_single_quote(gw.trim());
                s.push_str(&format!(
                    " && ({p}ip route replace default via {g} dev {d} || true)",
                    p = p,
                    g = qgw,
                    d = qdev
                ));
            }
            if let Some(dns_ip) = dns {
                let line = shell_single_quote(&format!("nameserver {}", dns_ip.trim()));
                s.push_str(&format!(
                    " && {p}sh -c \"printf '%s\\n' {l} > /etc/resolv.conf\"",
                    p = p,
                    l = line
                ));
            }
            s.push_str(&format!(" && ip -4 addr show dev {d}", d = qdev));
            s
        }
        other => {
            return Err(format!("Modo de red invitado inválido '{}' (usa dhcp o static)", other));
        }
    };
    run_ssh_command(target, &cmd, 20)
}

pub fn manage_service(
    target: &SshTarget,
    service: &str,
    action: &str,
) -> Result<SshResult, String> {
    let svc = validate_ident(service, "servicio")?;
    let act = action.trim().to_ascii_lowercase();
    if !matches!(
        act.as_str(),
        "start" | "stop" | "restart" | "reload" | "enable" | "disable" | "status"
    ) {
        return Err(format!("Acción de servicio inválida '{}'", action));
    }
    let qsvc = shell_single_quote(svc);
    let p = if act == "status" { "" } else { privileged_prefix(&target.user) };
    let cmd = format!(
        "if command -v systemctl >/dev/null 2>&1; then {p}systemctl --no-pager {act} {svc}; else {p}service {svc} {act}; fi",
        p = p,
        act = act,
        svc = qsvc
    );
    run_ssh_command(target, &cmd, 25)
}

pub fn install_package(target: &SshTarget, package: &str) -> Result<SshResult, String> {
    let pkg = validate_ident(package, "paquete")?;
    let qpkg = shell_single_quote(pkg);
    let p = privileged_prefix(&target.user);
    let cmd = format!(
        "if command -v apt-get >/dev/null 2>&1; then {p}env DEBIAN_FRONTEND=noninteractive apt-get update -qq && {p}env DEBIAN_FRONTEND=noninteractive apt-get install -y {pkg}; \
         elif command -v dnf >/dev/null 2>&1; then {p}dnf install -y {pkg}; \
         elif command -v pacman >/dev/null 2>&1; then {p}pacman -Sy --noconfirm {pkg}; \
         elif command -v apk >/dev/null 2>&1; then {p}apk add --no-cache {pkg}; \
         else echo 'Gestor de paquetes no soportado' >&2; exit 127; fi",
        p = p,
        pkg = qpkg
    );
    run_ssh_command(target, &cmd, 120)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_vm_host_guard() {
        assert!(validate_vm_host("127.0.0.1").is_ok());
        assert!(validate_vm_host("localhost").is_ok());
        assert!(validate_vm_host("10.0.2.15").is_ok());
        assert!(validate_vm_host("192.168.56.10").is_ok());
        assert!(validate_vm_host("172.16.0.5").is_ok());
        assert!(validate_vm_host("8.8.8.8").is_err());
        assert!(validate_vm_host("example.com").is_err());
    }

    #[test]
    fn test_validators_and_quoting() {
        assert_eq!(shell_single_quote("a'b"), "'a'\\''b'");
        assert!(validate_hostname("kali-vm-01").is_ok());
        assert!(validate_hostname("bad;host").is_err());
        assert!(validate_sysctl_key("net.ipv4.ip_forward").is_ok());
        assert!(validate_sysctl_key("bad key").is_err());
        assert!(validate_ident("ssh.service", "servicio").is_ok());
        assert!(validate_ident("nginx;rm", "servicio").is_err());
    }

    #[test]
    fn test_askpass_raii_cleanup() {
        let p = {
            let g = AskpassGuard::create("two55").expect("create askpass");
            assert!(g.path.exists());
            let meta = fs::metadata(&g.path).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o700);
            g.path.clone()
        };
        assert!(!p.exists(), "AskpassGuard debe borrar el script en Drop");
    }
}
