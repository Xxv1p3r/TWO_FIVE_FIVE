//! Agente autónomo de IA local (Ollama) para Two Five Five (255).
//!
//! Orquesta herramientas nativas (function-calling) sobre el hipervisor:
//! - Gestión de VMs: listar, crear, configurar, arrancar, detener y eliminar.
//! - Instalación desatendida automática de imágenes ISO (OEMDRV / CIDATA / Autounattend).
//! - Gestión de redes virtuales (NAT, Host-Only, Interna, TAP) y port-forwarding TCP.
//! - Inspección y modificación remota de parámetros dentro de los invitados por SSH.

use crate::gui_launcher::{
    detect_os_type_from_iso, friendly_vm_name_from_iso, launch_vm_external_with_networks,
    load_vm_profiles_from, next_free_ssh_port, save_vm_profiles_to, scan_disk_files,
    scan_iso_files, suggest_disk_filename, VmProfile,
};
use crate::json::JsonValue;
use crate::network::{
    format_ipv4, generate_mac, load_networks_from, next_available_subnet, parse_mac,
    save_networks_to, NetworkMode, PortForward, VirtualNetwork, DEFAULT_NETWORK_NAME,
    NETWORK_CONFIG_FILE,
};
use crate::ollama::{ChatMessage, OllamaClient};
use crate::ssh_client::{self, SshResult, SshTarget};
use crate::vdisk::VirtualDisk;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::mpsc;

const VM_CONFIG_FILE: &str = "vms.conf";
const MAX_TOOL_ROUNDS: usize = 8;

const SYSTEM_PROMPT: &str = "\
Eres el Agente IA Administrador integrado en el hipervisor Two Five Five (255) sobre KVM/Linux.
Responde siempre en español de forma clara, precisa y concisa.
Tienes herramientas reales para administrar:
1. Máquinas virtuales: `list_vms`, `create_vm`, `configure_vm`, `delete_vm`, `start_vm`, `stop_vm`.
2. Imágenes ISO e instalación desatendida: `list_isos`, `install_iso_auto`.
3. Redes virtuales y redirección de puertos: `list_networks`, `create_network`, `delete_network`, `assign_vm_network`, `manage_port_forward`.
4. Configuración de invitados por SSH: `ssh_get_vm_info`, `ssh_modify_vm`.

REGLAS ESTRICTAS:
- NUNCA inventes nombres de VMs, redes, IPs, puertos ni rutas de archivos: llama primero a `list_vms`, `list_networks` o `list_isos`.
- Cuando el usuario pida instalar una ISO automáticamente, usa `install_iso_auto` (asigna disco VDI, modo desatendido y puerto SSH redirigido).
- Cuando el usuario pida cambiar parámetros dentro de una VM Linux por SSH (hostname, sysctl, red/IP, servicios, paquetes o comandos), usa `ssh_get_vm_info` o `ssh_modify_vm`.
- Si una acción falla o la VM no responde aún por SSH, informa el motivo exacto sin ocultarlo.";

/// Estado mutable compartido con el que operan todas las herramientas del agente.
pub struct HypervisorState<'a> {
    pub work_dir: &'a Path,
    pub profiles: &'a mut Vec<VmProfile>,
    pub networks: &'a mut Vec<VirtualNetwork>,
    pub available_isos: &'a mut Vec<String>,
    pub available_disks: &'a mut Vec<String>,
    pub running_vms: &'a mut HashMap<String, Child>,
    pub selected_idx: Option<&'a mut usize>,
    pub status_msg: Option<&'a mut String>,
    pub status_is_err: Option<&'a mut bool>,
}

/// Peticiones síncronas de herramienta hacia el estado del hipervisor.
pub struct BackendOp {
    pub tool_name: String,
    pub args: JsonValue,
}

pub struct BackendEnvelope {
    pub op: BackendOp,
    pub reply_tx: mpsc::Sender<Result<String, String>>,
}

/// Abstracción del backend para compartir la ejecución entre GUI, CLI y pruebas unitarias.
pub trait AgentBackend: Send {
    fn execute_state_tool(&mut self, name: &str, args: &JsonValue) -> Result<String, String>;
    fn resolve_ssh_target(&mut self, args: &JsonValue) -> Result<SshTarget, String>;
}

/// Backend directo en memoria/archivo (para `--agent-prompt` CLI y pruebas unitarias).
pub struct DirectBackend {
    pub work_dir: PathBuf,
    pub profiles: Vec<VmProfile>,
    pub networks: Vec<VirtualNetwork>,
    pub available_isos: Vec<String>,
    pub available_disks: Vec<String>,
    pub running_vms: HashMap<String, Child>,
}

impl DirectBackend {
    pub fn open(work_dir: PathBuf) -> Self {
        let mut available_isos = scan_iso_files();
        // También buscar ISOs locales en work_dir si no coincide con cwd
        if let Ok(entries) = fs::read_dir(&work_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_file()
                    && p.extension()
                        .map(|e| e.eq_ignore_ascii_case("iso"))
                        .unwrap_or(false)
                {
                    let s = p.to_string_lossy().to_string();
                    if !available_isos.contains(&s) {
                        available_isos.push(s);
                    }
                }
            }
        }
        available_isos.sort();

        let available_disks = scan_disks_in_dir(&work_dir);
        let vm_conf = work_dir.join(VM_CONFIG_FILE);
        let net_conf = work_dir.join(NETWORK_CONFIG_FILE);
        let profiles = if vm_conf.exists() {
            load_vm_profiles_from(&vm_conf, &available_isos, &available_disks)
        } else {
            Vec::new()
        };
        let networks = load_networks_from(&net_conf);
        Self {
            work_dir,
            profiles,
            networks,
            available_isos,
            available_disks,
            running_vms: HashMap::new(),
        }
    }
}

impl AgentBackend for DirectBackend {
    fn execute_state_tool(&mut self, name: &str, args: &JsonValue) -> Result<String, String> {
        let mut state = HypervisorState {
            work_dir: &self.work_dir,
            profiles: &mut self.profiles,
            networks: &mut self.networks,
            available_isos: &mut self.available_isos,
            available_disks: &mut self.available_disks,
            running_vms: &mut self.running_vms,
            selected_idx: None,
            status_msg: None,
            status_is_err: None,
        };
        execute_state_op(name, args, &mut state)
    }

    fn resolve_ssh_target(&mut self, args: &JsonValue) -> Result<SshTarget, String> {
        resolve_ssh_target_from_profiles(args, &self.profiles)
    }
}

/// Backend basado en canal hacia el hilo principal del GUI (`minifb`).
pub struct ChannelBackend {
    req_tx: mpsc::Sender<BackendEnvelope>,
}

impl AgentBackend for ChannelBackend {
    fn execute_state_tool(&mut self, name: &str, args: &JsonValue) -> Result<String, String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.req_tx
            .send(BackendEnvelope {
                op: BackendOp {
                    tool_name: name.to_string(),
                    args: args.clone(),
                },
                reply_tx,
            })
            .map_err(|_| "El hilo de interfaz gráfica ya no está activo".to_string())?;
        reply_rx
            .recv()
            .map_err(|_| "No se recibió respuesta del gestor de estado del GUI".to_string())?
    }

    fn resolve_ssh_target(&mut self, args: &JsonValue) -> Result<SshTarget, String> {
        let raw = self.execute_state_tool("__resolve_ssh_target", args)?;
        let v = JsonValue::parse(&raw)?;
        Ok(SshTarget {
            host: v
                .get("host")
                .and_then(|x| x.as_str())
                .unwrap_or("127.0.0.1")
                .to_string(),
            port: v.get("port").and_then(|x| x.as_u64()).unwrap_or(22) as u16,
            user: v
                .get("user")
                .and_then(|x| x.as_str())
                .unwrap_or("two55")
                .to_string(),
            password: v
                .get("password")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
            key_path: v
                .get("key_path")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
        })
    }
}

fn scan_disks_in_dir(dir: &Path) -> Vec<String> {
    if dir == Path::new(".") {
        return scan_disk_files();
    }
    let mut disks = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() {
                if let Some(ext) = p.extension() {
                    let ext_s = ext.to_string_lossy().to_lowercase();
                    if matches!(ext_s.as_str(), "vdi" | "img" | "raw" | "vhd") {
                        if let Some(name) = p.file_name() {
                            let n = name.to_string_lossy().to_string();
                            if !n.contains("oemdrv") && !disks.contains(&n) {
                                disks.push(n);
                            }
                        }
                    }
                }
            }
        }
    }
    disks.sort();
    disks
}

fn set_status(st: &mut HypervisorState<'_>, msg: impl Into<String>, is_err: bool) {
    let m = msg.into();
    if let Some(ref mut s) = st.status_msg {
        **s = m;
    }
    if let Some(ref mut e) = st.status_is_err {
        **e = is_err;
    }
}

fn save_profiles_in(st: &HypervisorState<'_>) -> Result<(), String> {
    let path = st.work_dir.join(VM_CONFIG_FILE);
    save_vm_profiles_to(&path, st.profiles)
        .map_err(|e| format!("Error guardando '{}': {}", path.display(), e))
}

fn save_networks_in(st: &HypervisorState<'_>) -> Result<(), String> {
    let path = st.work_dir.join(NETWORK_CONFIG_FILE);
    save_networks_to(&path, st.networks)
        .map_err(|e| format!("Error guardando '{}': {}", path.display(), e))
}

fn find_vm_index(profiles: &[VmProfile], name: &str) -> Result<usize, String> {
    let q = name.trim();
    if q.is_empty() {
        return Err("Debes especificar el nombre de la VM".to_string());
    }
    if let Some(pos) = profiles.iter().position(|p| p.name.eq_ignore_ascii_case(q)) {
        return Ok(pos);
    }
    let q_low = q.to_ascii_lowercase();
    let matches: Vec<usize> = profiles
        .iter()
        .enumerate()
        .filter(|(_, p)| p.name.to_ascii_lowercase().contains(&q_low))
        .map(|(i, _)| i)
        .collect();
    if matches.len() == 1 {
        return Ok(matches[0]);
    }
    Err(format!("No se encontró una VM que coincida con '{}'", name))
}

fn match_iso_path(query: &str, available_isos: &[String]) -> Option<String> {
    let q = query.trim();
    if q.is_empty() {
        return None;
    }
    if Path::new(q).is_file() {
        return Some(q.to_string());
    }
    if let Some(exact) = available_isos.iter().find(|iso| {
        iso.eq_ignore_ascii_case(q)
            || Path::new(iso)
                .file_name()
                .map(|f| f.to_string_lossy().eq_ignore_ascii_case(q))
                .unwrap_or(false)
    }) {
        return Some(exact.clone());
    }
    let q_low = q.to_ascii_lowercase();
    available_isos
        .iter()
        .find(|iso| iso.to_ascii_lowercase().contains(&q_low))
        .cloned()
}

fn sanitize_safe_disk_filename(raw: &str) -> Result<String, String> {
    let fname = Path::new(raw.trim())
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .trim();
    if fname.is_empty() || fname.contains('/') || fname.contains('\\') || fname.starts_with('.') {
        return Err(format!("Nombre de archivo de disco inválido '{}'", raw));
    }
    if fname.to_ascii_lowercase().ends_with(".vdi") {
        Ok(fname.to_string())
    } else {
        Ok(format!("{}.vdi", fname))
    }
}

fn sync_ssh_port_to_network(net: &mut VirtualNetwork, host_port: u16) {
    if host_port > 0
        && !net
            .port_forwards
            .iter()
            .any(|pf| pf.host_port == host_port && pf.guest_port == 22)
    {
        net.port_forwards.push(PortForward {
            host_port,
            guest_port: 22,
        });
    }
}

fn resolve_ssh_target_from_profiles(
    args: &JsonValue,
    profiles: &[VmProfile],
) -> Result<SshTarget, String> {
    let profile = args
        .get("vm_name")
        .and_then(|v| v.as_str())
        .and_then(|name| find_vm_index(profiles, name).ok())
        .map(|idx| &profiles[idx]);

    let host = args
        .get("host")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "127.0.0.1".to_string());

    let port = args
        .get("port")
        .and_then(|v| v.as_u64())
        .map(|p| p as u16)
        .or_else(|| profile.map(|p| p.ssh_host_port).filter(|&p| p > 0))
        .ok_or_else(|| {
            "No se especificó puerto SSH ni la VM tiene `ssh_host_port` configurado (usa `configure_vm` o `assign_vm_network` para asignarlo)".to_string()
        })?;

    let user = args
        .get("user")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| profile.map(|p| p.ssh_user.clone()))
        .unwrap_or_else(|| "two55".to_string());

    let key_path = args
        .get("key_path")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty());

    let password = if key_path.is_some() {
        None
    } else {
        args.get("password")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| profile.map(|p| p.ssh_pass.clone()))
            .or_else(|| Some("two55".to_string()))
    };

    Ok(SshTarget {
        host,
        port,
        user,
        password,
        key_path,
    })
}

/// Ejecuta cualquier herramienta de estado sobre `HypervisorState`.
/// Compartido entre GUI (`ChannelBackend`), CLI (`DirectBackend`) y pruebas.
pub fn execute_state_op(
    name: &str,
    args: &JsonValue,
    st: &mut HypervisorState<'_>,
) -> Result<String, String> {
    // Limpiar procesos hijos ya finalizados
    let mut finished = Vec::new();
    for (vm_name, child) in st.running_vms.iter_mut() {
        if let Ok(Some(_)) = child.try_wait() {
            finished.push(vm_name.clone());
        }
    }
    for vm_name in finished {
        st.running_vms.remove(&vm_name);
    }

    match name {
        "__resolve_ssh_target" => {
            let t = resolve_ssh_target_from_profiles(args, st.profiles)?;
            let mut entries = vec![
                ("host", JsonValue::Str(t.host)),
                ("port", JsonValue::Number(t.port as f64)),
                ("user", JsonValue::Str(t.user)),
            ];
            if let Some(p) = t.password {
                entries.push(("password", JsonValue::Str(p)));
            }
            if let Some(k) = t.key_path {
                entries.push(("key_path", JsonValue::Str(k)));
            }
            Ok(JsonValue::obj(entries).to_json_string())
        }

        "list_vms" => {
            let arr: Vec<JsonValue> = st
                .profiles
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    JsonValue::obj(vec![
                        ("index", JsonValue::Number(i as f64)),
                        ("name", JsonValue::Str(p.name.clone())),
                        ("os_type", JsonValue::Str(p.os_type.clone())),
                        ("cpus", JsonValue::Number(p.cpus as f64)),
                        ("ram_mb", JsonValue::Number(p.ram_mb as f64)),
                        (
                            "iso_path",
                            p.iso_path
                                .as_ref()
                                .map(|s| JsonValue::Str(s.clone()))
                                .unwrap_or(JsonValue::Null),
                        ),
                        (
                            "disk_path",
                            p.disk_path
                                .as_ref()
                                .map(|s| JsonValue::Str(s.clone()))
                                .unwrap_or(JsonValue::Null),
                        ),
                        ("network", JsonValue::Str(p.network_name.clone())),
                        ("mac_address", JsonValue::Str(p.mac_address.clone())),
                        ("ssh_host_port", JsonValue::Number(p.ssh_host_port as f64)),
                        ("ssh_user", JsonValue::Str(p.ssh_user.clone())),
                        ("unattended", JsonValue::Bool(p.unattended)),
                        ("use_efi", JsonValue::Bool(p.use_efi)),
                        ("running", JsonValue::Bool(st.running_vms.contains_key(&p.name))),
                    ])
                })
                .collect();
            Ok(JsonValue::obj(vec![
                ("count", JsonValue::Number(arr.len() as f64)),
                ("vms", JsonValue::Array(arr)),
            ])
            .to_json_string())
        }

        "create_vm" => {
            let vm_name = args
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().replace('|', "/"))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "create_vm requiere parámetro 'name'".to_string())?;
            if st
                .profiles
                .iter()
                .any(|p| p.name.eq_ignore_ascii_case(&vm_name))
            {
                return Err(format!("Ya existe una VM llamada '{}'", vm_name));
            }

            let cpus = args
                .get("cpus")
                .and_then(|v| v.as_u64())
                .unwrap_or(2)
                .clamp(1, 16) as u32;
            let ram_mb = args
                .get("ram_mb")
                .and_then(|v| v.as_u64())
                .unwrap_or(2048)
                .clamp(512, 16384) as usize;
            let iso_path = args
                .get("iso")
                .and_then(|v| v.as_str())
                .and_then(|q| match_iso_path(q, st.available_isos));
            let create_disk = args
                .get("create_disk")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let disk_gb = args
                .get("disk_gb")
                .and_then(|v| v.as_u64())
                .unwrap_or(20)
                .clamp(1, 500);
            let network_name = args
                .get("network")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_NETWORK_NAME.to_string());
            if !st.networks.iter().any(|n| n.name == network_name) {
                return Err(format!(
                    "La red virtual '{}' no existe. Créala primero con `create_network`.",
                    network_name
                ));
            }
            let ssh_port = match args.get("ssh_host_port").and_then(|v| v.as_u64()) {
                Some(0) | None => next_free_ssh_port(st.profiles),
                Some(p) => p.min(65535) as u16,
            };
            let ssh_user = args
                .get("ssh_user")
                .and_then(|v| v.as_str())
                .unwrap_or("two55")
                .to_string();
            let ssh_pass = args
                .get("ssh_pass")
                .and_then(|v| v.as_str())
                .unwrap_or("two55")
                .to_string();
            let unattended = args
                .get("unattended")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let use_efi = args
                .get("use_efi")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let start_now = args
                .get("start_now")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let disk_path = if create_disk {
                let fname = sanitize_safe_disk_filename(&suggest_disk_filename(&vm_name, ".vdi"))?;
                let disk_full = st.work_dir.join(&fname);
                if !disk_full.exists() {
                    VirtualDisk::create_vdi(
                        &disk_full,
                        disk_gb * 1024 * 1024 * 1024,
                        false,
                        "Created by Two Five Five (255) AI Agent",
                    )
                    .map_err(|e| format!("Error creando VDI '{}': {}", disk_full.display(), e))?;
                }
                let stored = if st.work_dir == Path::new(".") {
                    fname
                } else {
                    disk_full.to_string_lossy().to_string()
                };
                if !st.available_disks.contains(&stored) {
                    st.available_disks.push(stored.clone());
                }
                Some(stored)
            } else {
                None
            };

            let mut prof = VmProfile::new_default(&vm_name, iso_path.clone(), disk_path.clone());
            if let Some(ref i) = iso_path {
                prof.os_type = detect_os_type_from_iso(i);
            }
            prof.cpus = cpus;
            prof.ram_mb = ram_mb;
            prof.network_name = network_name.clone();
            prof.mac_address = generate_mac(&vm_name);
            prof.ssh_host_port = ssh_port;
            prof.ssh_user = ssh_user;
            prof.ssh_pass = ssh_pass;
            prof.unattended = unattended;
            prof.use_efi = use_efi;

            if let Some(net) = st.networks.iter_mut().find(|n| n.name == network_name) {
                sync_ssh_port_to_network(net, ssh_port);
            }
            st.profiles.push(prof);
            let idx = st.profiles.len() - 1;
            if let Some(ref mut sel) = st.selected_idx {
                **sel = idx;
            }
            save_networks_in(st)?;
            save_profiles_in(st)?;

            let mut launch_note = String::new();
            if start_now {
                let mut msg = String::new();
                let mut err = false;
                launch_vm_external_with_networks(
                    &st.profiles[idx],
                    st.networks,
                    st.running_vms,
                    &mut msg,
                    &mut err,
                );
                launch_note = msg;
            }
            set_status(st, format!("IA: VM '{}' creada.", vm_name), false);

            Ok(JsonValue::obj(vec![
                ("ok", JsonValue::Bool(true)),
                ("name", JsonValue::Str(vm_name)),
                ("cpus", JsonValue::Number(cpus as f64)),
                ("ram_mb", JsonValue::Number(ram_mb as f64)),
                (
                    "disk_path",
                    disk_path.map(JsonValue::Str).unwrap_or(JsonValue::Null),
                ),
                (
                    "iso_path",
                    iso_path.map(JsonValue::Str).unwrap_or(JsonValue::Null),
                ),
                ("network", JsonValue::Str(network_name)),
                ("ssh_host_port", JsonValue::Number(ssh_port as f64)),
                ("started", JsonValue::Bool(start_now)),
                ("launch_note", JsonValue::Str(launch_note)),
            ])
            .to_json_string())
        }

        "delete_vm" => {
            let vm_name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "delete_vm requiere parámetro 'name'".to_string())?;
            let delete_disk = args
                .get("delete_disk")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let force_stop = args
                .get("force_stop")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);

            let idx = find_vm_index(st.profiles, vm_name)?;
            let exact_name = st.profiles[idx].name.clone();

            if st.running_vms.contains_key(&exact_name) {
                if !force_stop {
                    return Err(format!(
                        "La VM '{}' está en ejecución; usa force_stop=true o deténla primero.",
                        exact_name
                    ));
                }
                if let Some(mut child) = st.running_vms.remove(&exact_name) {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }

            let removed = st.profiles.remove(idx);
            if let Some(ref mut sel) = st.selected_idx {
                if st.profiles.is_empty() {
                    **sel = 0;
                } else if **sel >= st.profiles.len() {
                    **sel = st.profiles.len() - 1;
                }
            }

            let mut disk_deleted = false;
            if delete_disk {
                if let Some(ref dpath) = removed.disk_path {
                    let shared = st
                        .profiles
                        .iter()
                        .any(|p| p.disk_path.as_deref() == Some(dpath.as_str()));
                    if !shared {
                        let candidate = if Path::new(dpath).is_absolute() {
                            PathBuf::from(dpath)
                        } else {
                            st.work_dir.join(dpath)
                        };
                        if let (Ok(canon_dir), Ok(canon_file)) =
                            (fs::canonicalize(st.work_dir), fs::canonicalize(&candidate))
                        {
                            if canon_file.starts_with(&canon_dir) && canon_file.is_file() {
                                if fs::remove_file(&canon_file).is_ok() {
                                    disk_deleted = true;
                                    st.available_disks.retain(|d| d != dpath);
                                }
                            }
                        }
                    }
                }
            }

            save_profiles_in(st)?;
            set_status(st, format!("IA: VM '{}' eliminada.", removed.name), false);
            Ok(JsonValue::obj(vec![
                ("ok", JsonValue::Bool(true)),
                ("deleted_vm", JsonValue::Str(removed.name)),
                ("disk_deleted", JsonValue::Bool(disk_deleted)),
                ("remaining_vms", JsonValue::Number(st.profiles.len() as f64)),
            ])
            .to_json_string())
        }

        "configure_vm" => {
            let vm_name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "configure_vm requiere parámetro 'name'".to_string())?;
            let idx = find_vm_index(st.profiles, vm_name)?;

            if let Some(net_name) = args.get("network").and_then(|v| v.as_str()) {
                if !st.networks.iter().any(|n| n.name == net_name.trim()) {
                    return Err(format!("Red virtual '{}' no encontrada", net_name));
                }
                st.profiles[idx].network_name = net_name.trim().to_string();
            }
            if let Some(new_name) = args.get("new_name").and_then(|v| v.as_str()) {
                let clean = new_name.trim().replace('|', "/");
                if !clean.is_empty() {
                    st.profiles[idx].name = clean;
                }
            }
            if let Some(cpus) = args.get("cpus").and_then(|v| v.as_u64()) {
                st.profiles[idx].cpus = (cpus as u32).clamp(1, 16);
            }
            if let Some(ram) = args.get("ram_mb").and_then(|v| v.as_u64()) {
                st.profiles[idx].ram_mb = (ram as usize).clamp(512, 16384);
            }
            if let Some(iso_q) = args.get("iso").and_then(|v| v.as_str()) {
                if iso_q.trim().is_empty() || iso_q.eq_ignore_ascii_case("none") {
                    st.profiles[idx].iso_path = None;
                } else {
                    let matched = match_iso_path(iso_q, st.available_isos)
                        .ok_or_else(|| format!("ISO '{}' no encontrada", iso_q))?;
                    st.profiles[idx].os_type = detect_os_type_from_iso(&matched);
                    st.profiles[idx].iso_path = Some(matched);
                }
            }
            if let Some(unatt) = args.get("unattended").and_then(|v| v.as_bool()) {
                st.profiles[idx].unattended = unatt;
            }
            if let Some(efi) = args.get("use_efi").and_then(|v| v.as_bool()) {
                st.profiles[idx].use_efi = efi;
            }
            if let Some(mac) = args.get("mac_address").and_then(|v| v.as_str()) {
                parse_mac(mac).ok_or_else(|| format!("MAC inválida '{}'", mac))?;
                st.profiles[idx].mac_address = mac.trim().to_string();
            }
            if let Some(port) = args.get("ssh_host_port").and_then(|v| v.as_u64()) {
                st.profiles[idx].ssh_host_port = port.min(65535) as u16;
                let net_name = st.profiles[idx].network_name.clone();
                if let Some(net) = st.networks.iter_mut().find(|n| n.name == net_name) {
                    sync_ssh_port_to_network(net, st.profiles[idx].ssh_host_port);
                }
                save_networks_in(st)?;
            }
            if let Some(u) = args.get("ssh_user").and_then(|v| v.as_str()) {
                st.profiles[idx].ssh_user = u.trim().to_string();
            }
            if let Some(pw) = args.get("ssh_pass").and_then(|v| v.as_str()) {
                st.profiles[idx].ssh_pass = pw.to_string();
            }

            save_profiles_in(st)?;
            let summary = {
                let p = &st.profiles[idx];
                JsonValue::obj(vec![
                    ("ok", JsonValue::Bool(true)),
                    ("name", JsonValue::Str(p.name.clone())),
                    ("cpus", JsonValue::Number(p.cpus as f64)),
                    ("ram_mb", JsonValue::Number(p.ram_mb as f64)),
                    ("network", JsonValue::Str(p.network_name.clone())),
                    ("ssh_host_port", JsonValue::Number(p.ssh_host_port as f64)),
                    ("unattended", JsonValue::Bool(p.unattended)),
                    ("use_efi", JsonValue::Bool(p.use_efi)),
                ])
                .to_json_string()
            };
            let vm_name_msg = st.profiles[idx].name.clone();
            set_status(st, format!("IA: VM '{}' actualizada.", vm_name_msg), false);
            Ok(summary)
        }

        "start_vm" => {
            let vm_name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "start_vm requiere parámetro 'name'".to_string())?;
            let idx = find_vm_index(st.profiles, vm_name)?;
            let mut msg = String::new();
            let mut err = false;
            launch_vm_external_with_networks(
                &st.profiles[idx],
                st.networks,
                st.running_vms,
                &mut msg,
                &mut err,
            );
            set_status(st, msg.clone(), err);
            if err {
                Err(msg)
            } else {
                Ok(JsonValue::obj(vec![
                    ("ok", JsonValue::Bool(true)),
                    ("vm", JsonValue::Str(st.profiles[idx].name.clone())),
                    ("message", JsonValue::Str(msg)),
                ])
                .to_json_string())
            }
        }

        "stop_vm" => {
            let vm_name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "stop_vm requiere parámetro 'name'".to_string())?;
            let idx = find_vm_index(st.profiles, vm_name)?;
            let exact = st.profiles[idx].name.clone();
            if let Some(mut child) = st.running_vms.remove(&exact) {
                unsafe {
                    libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
                if let Ok(None) = child.try_wait() {
                    let _ = child.kill();
                }
                let _ = child.wait();
                let msg = format!("VM '{}' detenida.", exact);
                set_status(st, msg.clone(), false);
                Ok(JsonValue::obj(vec![
                    ("ok", JsonValue::Bool(true)),
                    ("message", JsonValue::Str(msg)),
                ])
                .to_json_string())
            } else {
                Err(format!("La VM '{}' no está en ejecución actualmente", exact))
            }
        }

        "list_isos" => {
            let arr: Vec<JsonValue> = st
                .available_isos
                .iter()
                .map(|iso| {
                    let size_mb = fs::metadata(iso)
                        .map(|m| m.len() / (1024 * 1024))
                        .unwrap_or(0);
                    JsonValue::obj(vec![
                        ("path", JsonValue::Str(iso.clone())),
                        ("os_type", JsonValue::Str(detect_os_type_from_iso(iso))),
                        ("size_mb", JsonValue::Number(size_mb as f64)),
                    ])
                })
                .collect();
            Ok(JsonValue::obj(vec![
                ("count", JsonValue::Number(arr.len() as f64)),
                ("isos", JsonValue::Array(arr)),
            ])
            .to_json_string())
        }

        "install_iso_auto" => {
            let iso_q = args
                .get("iso")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "install_iso_auto requiere parámetro 'iso'".to_string())?;
            let iso_path = match_iso_path(iso_q, st.available_isos)
                .ok_or_else(|| format!("No se encontró ninguna ISO que coincida con '{}'", iso_q))?;
            let cpus = args
                .get("cpus")
                .and_then(|v| v.as_u64())
                .unwrap_or(2)
                .clamp(1, 16) as u32;
            let ram_mb = args
                .get("ram_mb")
                .and_then(|v| v.as_u64())
                .unwrap_or(2048)
                .clamp(512, 16384) as usize;
            let disk_gb = args
                .get("disk_gb")
                .and_then(|v| v.as_u64())
                .unwrap_or(20)
                .clamp(2, 500);
            let username = args
                .get("username")
                .and_then(|v| v.as_str())
                .unwrap_or("two55")
                .to_string();
            let password = args
                .get("password")
                .and_then(|v| v.as_str())
                .unwrap_or("two55")
                .to_string();
            let network_name = args
                .get("network")
                .and_then(|v| v.as_str())
                .unwrap_or(DEFAULT_NETWORK_NAME)
                .to_string();
            if !st.networks.iter().any(|n| n.name == network_name) {
                return Err(format!("La red '{}' no existe", network_name));
            }
            let use_efi = args
                .get("use_efi")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let start_now = args
                .get("start_now")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);

            let desired_name = args
                .get("vm_name")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| friendly_vm_name_from_iso(&iso_path, st.profiles.len()));

            let disk_fname = sanitize_safe_disk_filename(&suggest_disk_filename(&desired_name, ".vdi"))?;
            let disk_full = st.work_dir.join(&disk_fname);
            if !disk_full.exists() {
                VirtualDisk::create_vdi(
                    &disk_full,
                    disk_gb * 1024 * 1024 * 1024,
                    false,
                    "Created for Unattended ISO Install (Two Five Five 255)",
                )
                .map_err(|e| format!("Error creando VDI '{}': {}", disk_full.display(), e))?;
            }
            let stored_disk = if st.work_dir == Path::new(".") {
                disk_fname
            } else {
                disk_full.to_string_lossy().to_string()
            };
            if !st.available_disks.contains(&stored_disk) {
                st.available_disks.push(stored_disk.clone());
            }

            let ssh_port = match args.get("ssh_host_port").and_then(|v| v.as_u64()) {
                Some(0) | None => next_free_ssh_port(st.profiles),
                Some(p) => p.min(65535) as u16,
            };

            let idx = if let Ok(existing) = find_vm_index(st.profiles, &desired_name) {
                let p = &mut st.profiles[existing];
                p.os_type = detect_os_type_from_iso(&iso_path);
                p.iso_path = Some(iso_path.clone());
                if p.disk_path.is_none() {
                    p.disk_path = Some(stored_disk.clone());
                }
                p.cpus = cpus;
                p.ram_mb = ram_mb;
                p.unattended = true;
                p.use_efi = use_efi;
                p.network_name = network_name.clone();
                if p.ssh_host_port == 0 {
                    p.ssh_host_port = ssh_port;
                }
                p.ssh_user = username.clone();
                p.ssh_pass = password.clone();
                existing
            } else {
                let mut prof = VmProfile::new_default(
                    &desired_name,
                    Some(iso_path.clone()),
                    Some(stored_disk.clone()),
                );
                prof.os_type = detect_os_type_from_iso(&iso_path);
                prof.cpus = cpus;
                prof.ram_mb = ram_mb;
                prof.unattended = true;
                prof.use_efi = use_efi;
                prof.network_name = network_name.clone();
                prof.mac_address = generate_mac(&desired_name);
                prof.ssh_host_port = ssh_port;
                prof.ssh_user = username.clone();
                prof.ssh_pass = password.clone();
                st.profiles.push(prof);
                st.profiles.len() - 1
            };

            let used_ssh_port = st.profiles[idx].ssh_host_port;
            if let Some(net) = st.networks.iter_mut().find(|n| n.name == network_name) {
                sync_ssh_port_to_network(net, used_ssh_port);
            }
            if let Some(ref mut sel) = st.selected_idx {
                **sel = idx;
            }
            save_networks_in(st)?;
            save_profiles_in(st)?;

            let mut launch_msg = String::new();
            if start_now {
                let mut err = false;
                launch_vm_external_with_networks(
                    &st.profiles[idx],
                    st.networks,
                    st.running_vms,
                    &mut launch_msg,
                    &mut err,
                );
            }
            set_status(
                st,
                format!("IA: Instalación desatendida lista para '{}'.", desired_name),
                false,
            );
            Ok(JsonValue::obj(vec![
                ("ok", JsonValue::Bool(true)),
                ("vm_name", JsonValue::Str(st.profiles[idx].name.clone())),
                ("iso_path", JsonValue::Str(iso_path)),
                ("disk_path", JsonValue::Str(stored_disk)),
                ("unattended", JsonValue::Bool(true)),
                ("ssh_host_port", JsonValue::Number(used_ssh_port as f64)),
                ("ssh_user", JsonValue::Str(username)),
                ("started", JsonValue::Bool(start_now)),
                ("launch_note", JsonValue::Str(launch_msg)),
            ])
            .to_json_string())
        }

        "list_networks" => {
            let arr: Vec<JsonValue> = st
                .networks
                .iter()
                .map(|n| {
                    let attached: Vec<JsonValue> = st
                        .profiles
                        .iter()
                        .filter(|p| p.network_name == n.name)
                        .map(|p| JsonValue::Str(p.name.clone()))
                        .collect();
                    let forwards: Vec<JsonValue> = n
                        .port_forwards
                        .iter()
                        .map(|pf| JsonValue::Str(pf.to_string()))
                        .collect();
                    JsonValue::obj(vec![
                        ("name", JsonValue::Str(n.name.clone())),
                        ("mode", JsonValue::Str(n.mode.as_str().to_string())),
                        ("subnet", JsonValue::Str(n.subnet.clone())),
                        ("gateway", JsonValue::Str(format_ipv4(n.gateway_ip))),
                        ("guest_ip", JsonValue::Str(format_ipv4(n.guest_ip))),
                        ("dns", JsonValue::Str(format_ipv4(n.dns_ip))),
                        (
                            "tap_if",
                            n.tap_if
                                .as_ref()
                                .map(|s| JsonValue::Str(s.clone()))
                                .unwrap_or(JsonValue::Null),
                        ),
                        ("port_forwards", JsonValue::Array(forwards)),
                        ("attached_vms", JsonValue::Array(attached)),
                    ])
                })
                .collect();
            Ok(JsonValue::obj(vec![
                ("count", JsonValue::Number(arr.len() as f64)),
                ("networks", JsonValue::Array(arr)),
            ])
            .to_json_string())
        }

        "create_network" => {
            let net_name = args
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "create_network requiere parámetro 'name'".to_string())?;
            if st
                .networks
                .iter()
                .any(|n| n.name.eq_ignore_ascii_case(&net_name))
            {
                return Err(format!("La red '{}' ya existe", net_name));
            }
            let mode = args
                .get("mode")
                .and_then(|v| v.as_str())
                .map(NetworkMode::parse)
                .transpose()?
                .unwrap_or(NetworkMode::Nat);
            let subnet = args
                .get("subnet")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| next_available_subnet(st.networks));
            let tap_if = args
                .get("tap_if")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let mut forwards = Vec::new();
            if let Some(arr) = args.get("port_forwards").and_then(|v| v.as_array()) {
                for item in arr {
                    if let Some(s) = item.as_str() {
                        forwards.push(PortForward::parse(s)?);
                    }
                }
            }
            let net = VirtualNetwork::from_cidr(&net_name, mode, &subnet, tap_if, forwards)?;
            let summary = JsonValue::obj(vec![
                ("ok", JsonValue::Bool(true)),
                ("name", JsonValue::Str(net.name.clone())),
                ("mode", JsonValue::Str(net.mode.as_str().to_string())),
                ("subnet", JsonValue::Str(net.subnet.clone())),
                ("gateway", JsonValue::Str(format_ipv4(net.gateway_ip))),
                ("guest_ip", JsonValue::Str(format_ipv4(net.guest_ip))),
            ]);
            st.networks.push(net);
            save_networks_in(st)?;
            set_status(st, format!("IA: Red '{}' creada.", net_name), false);
            Ok(summary.to_json_string())
        }

        "delete_network" => {
            let net_name = args
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.trim())
                .ok_or_else(|| "delete_network requiere parámetro 'name'".to_string())?;
            if net_name.eq_ignore_ascii_case(DEFAULT_NETWORK_NAME) {
                return Err(format!("No se puede eliminar la red base '{}'", DEFAULT_NETWORK_NAME));
            }
            let pos = st
                .networks
                .iter()
                .position(|n| n.name.eq_ignore_ascii_case(net_name))
                .ok_or_else(|| format!("Red '{}' no encontrada", net_name))?;
            let removed = st.networks.remove(pos);
            let mut reassigned = 0usize;
            for p in st.profiles.iter_mut() {
                if p.network_name.eq_ignore_ascii_case(&removed.name) {
                    p.network_name = DEFAULT_NETWORK_NAME.to_string();
                    reassigned += 1;
                }
            }
            save_networks_in(st)?;
            if reassigned > 0 {
                save_profiles_in(st)?;
            }
            set_status(st, format!("IA: Red '{}' eliminada.", removed.name), false);
            Ok(JsonValue::obj(vec![
                ("ok", JsonValue::Bool(true)),
                ("deleted_network", JsonValue::Str(removed.name)),
                ("reassigned_vms", JsonValue::Number(reassigned as f64)),
            ])
            .to_json_string())
        }

        "assign_vm_network" => {
            let vm_name = args
                .get("vm_name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "assign_vm_network requiere 'vm_name'".to_string())?;
            let net_name = args
                .get("network_name")
                .and_then(|v| v.as_str())
                .map(|s| s.trim())
                .ok_or_else(|| "assign_vm_network requiere 'network_name'".to_string())?;
            let net_pos = st
                .networks
                .iter()
                .position(|n| n.name.eq_ignore_ascii_case(net_name))
                .ok_or_else(|| format!("Red virtual '{}' no encontrada", net_name))?;
            let exact_net = st.networks[net_pos].name.clone();
            let vm_idx = find_vm_index(st.profiles, vm_name)?;
            st.profiles[vm_idx].network_name = exact_net.clone();
            if args
                .get("regenerate_mac")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                let seed = format!("{}-{}", st.profiles[vm_idx].name, exact_net);
                st.profiles[vm_idx].mac_address = generate_mac(&seed);
            }
            if let Some(port) = args.get("ssh_host_port").and_then(|v| v.as_u64()) {
                let hp = port.min(65535) as u16;
                st.profiles[vm_idx].ssh_host_port = hp;
                sync_ssh_port_to_network(&mut st.networks[net_pos], hp);
                save_networks_in(st)?;
            }
            save_profiles_in(st)?;
            set_status(
                st,
                format!("IA: '{}' conectada a red '{}'.", st.profiles[vm_idx].name, exact_net),
                false,
            );
            Ok(JsonValue::obj(vec![
                ("ok", JsonValue::Bool(true)),
                ("vm_name", JsonValue::Str(st.profiles[vm_idx].name.clone())),
                ("network_name", JsonValue::Str(exact_net)),
                ("mac_address", JsonValue::Str(st.profiles[vm_idx].mac_address.clone())),
                ("ssh_host_port", JsonValue::Number(st.profiles[vm_idx].ssh_host_port as f64)),
            ])
            .to_json_string())
        }

        "manage_port_forward" => {
            let action = args
                .get("action")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "manage_port_forward requiere 'action' (add o remove)".to_string())?;
            let net_name = args
                .get("network_name")
                .and_then(|v| v.as_str())
                .unwrap_or(DEFAULT_NETWORK_NAME);
            let host_port = args
                .get("host_port")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| "Falta host_port".to_string())? as u16;
            let guest_port = args
                .get("guest_port")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| "Falta guest_port".to_string())? as u16;
            if host_port == 0 || guest_port == 0 {
                return Err("host_port y guest_port deben ser > 0".to_string());
            }
            let pos = st
                .networks
                .iter()
                .position(|n| n.name.eq_ignore_ascii_case(net_name.trim()))
                .ok_or_else(|| format!("Red '{}' no encontrada", net_name))?;
            let pf = PortForward {
                host_port,
                guest_port,
            };
            match action.trim().to_ascii_lowercase().as_str() {
                "add" => {
                    if !st.networks[pos].port_forwards.contains(&pf) {
                        st.networks[pos].port_forwards.push(pf);
                    }
                }
                "remove" => {
                    st.networks[pos].port_forwards.retain(|x| *x != pf);
                }
                other => return Err(format!("Acción inválida '{}' (usa add o remove)", other)),
            }
            save_networks_in(st)?;
            set_status(
                st,
                format!("IA: Port-forward {} en '{}'.", pf, st.networks[pos].name),
                false,
            );
            let all_pf: Vec<JsonValue> = st.networks[pos]
                .port_forwards
                .iter()
                .map(|x| JsonValue::Str(x.to_string()))
                .collect();
            Ok(JsonValue::obj(vec![
                ("ok", JsonValue::Bool(true)),
                ("network", JsonValue::Str(st.networks[pos].name.clone())),
                ("port_forwards", JsonValue::Array(all_pf)),
            ])
            .to_json_string())
        }

        _ => Err(format!("Herramienta desconocida '{}'", name)),
    }
}

fn ssh_result_to_json(res: SshResult) -> String {
    JsonValue::obj(vec![
        ("success", JsonValue::Bool(res.success)),
        ("exit_code", JsonValue::Number(res.exit_code as f64)),
        ("stdout", JsonValue::Str(res.stdout)),
        ("stderr", JsonValue::Str(res.stderr)),
    ])
    .to_json_string()
}

/// Despacha cualquier herramienta (tanto de estado local como remotas por SSH).
pub fn dispatch_tool(
    backend: &mut dyn AgentBackend,
    name: &str,
    args: &JsonValue,
) -> Result<String, String> {
    match name {
        "ssh_get_vm_info" => {
            let target = backend.resolve_ssh_target(args)?;
            let res = ssh_client::get_vm_info(&target)?;
            Ok(ssh_result_to_json(res))
        }
        "ssh_modify_vm" => {
            let target = backend.resolve_ssh_target(args)?;
            let action = args
                .get("action")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "ssh_modify_vm requiere parámetro 'action'".to_string())?;
            let res = match action.trim().to_ascii_lowercase().as_str() {
                "set_hostname" => {
                    let h = args
                        .get("hostname")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| "action=set_hostname requiere 'hostname'".to_string())?;
                    ssh_client::set_hostname(&target, h)?
                }
                "set_sysctl" => {
                    let k = args
                        .get("sysctl_key")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| "action=set_sysctl requiere 'sysctl_key'".to_string())?;
                    let v = args
                        .get("sysctl_value")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| "action=set_sysctl requiere 'sysctl_value'".to_string())?;
                    let persist = args.get("persist").and_then(|v| v.as_bool()).unwrap_or(true);
                    ssh_client::set_sysctl(&target, k, v, persist)?
                }
                "configure_network" => {
                    let iface = args
                        .get("iface")
                        .and_then(|v| v.as_str())
                        .unwrap_or("eth0");
                    let mode = args
                        .get("net_mode")
                        .and_then(|v| v.as_str())
                        .unwrap_or("dhcp");
                    let ip_cidr = args.get("ip_cidr").and_then(|v| v.as_str());
                    let gw = args.get("gateway").and_then(|v| v.as_str());
                    let dns = args.get("dns").and_then(|v| v.as_str());
                    ssh_client::configure_guest_network(&target, iface, mode, ip_cidr, gw, dns)?
                }
                "manage_service" => {
                    let svc = args
                        .get("service")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| "action=manage_service requiere 'service'".to_string())?;
                    let act = args
                        .get("service_action")
                        .and_then(|v| v.as_str())
                        .unwrap_or("status");
                    ssh_client::manage_service(&target, svc, act)?
                }
                "install_package" => {
                    let pkg = args
                        .get("package")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| "action=install_package requiere 'package'".to_string())?;
                    ssh_client::install_package(&target, pkg)?
                }
                "exec_command" => {
                    let cmd = args
                        .get("command")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| "action=exec_command requiere 'command'".to_string())?;
                    ssh_client::run_ssh_command(&target, cmd, 30)?
                }
                other => return Err(format!("Acción SSH desconocida '{}'", other)),
            };
            Ok(ssh_result_to_json(res))
        }
        other => backend.execute_state_tool(other, args),
    }
}

fn tool_schema(
    name: &str,
    description: &str,
    properties: Vec<(&str, JsonValue)>,
    required: Vec<&str>,
) -> JsonValue {
    JsonValue::obj(vec![
        ("type", JsonValue::Str("function".to_string())),
        (
            "function",
            JsonValue::obj(vec![
                ("name", JsonValue::Str(name.to_string())),
                ("description", JsonValue::Str(description.to_string())),
                (
                    "parameters",
                    JsonValue::obj(vec![
                        ("type", JsonValue::Str("object".to_string())),
                        ("properties", JsonValue::obj(properties)),
                        (
                            "required",
                            JsonValue::Array(
                                required
                                    .into_iter()
                                    .map(|r| JsonValue::Str(r.to_string()))
                                    .collect(),
                            ),
                        ),
                    ]),
                ),
            ]),
        ),
    ])
}

fn prop_str(desc: &str) -> JsonValue {
    JsonValue::obj(vec![
        ("type", JsonValue::Str("string".to_string())),
        ("description", JsonValue::Str(desc.to_string())),
    ])
}

fn prop_int(desc: &str) -> JsonValue {
    JsonValue::obj(vec![
        ("type", JsonValue::Str("integer".to_string())),
        ("description", JsonValue::Str(desc.to_string())),
    ])
}

fn prop_bool(desc: &str) -> JsonValue {
    JsonValue::obj(vec![
        ("type", JsonValue::Str("boolean".to_string())),
        ("description", JsonValue::Str(desc.to_string())),
    ])
}

/// Esquemas de todas las herramientas expuestas al modelo Ollama.
pub fn agent_tool_schemas() -> Vec<JsonValue> {
    vec![
        tool_schema(
            "list_vms",
            "Lista todas las máquinas virtuales configuradas en vms.conf con su estado, CPU, RAM, disco, ISO, red y puerto SSH.",
            vec![],
            vec![],
        ),
        tool_schema(
            "create_vm",
            "Crea una nueva máquina virtual en vms.conf y opcionalmente su disco VDI y arranque inmediato.",
            vec![
                ("name", prop_str("Nombre de la nueva máquina virtual")),
                ("cpus", prop_int("Número de vCPUs (1..16, defecto 2)")),
                ("ram_mb", prop_int("Memoria RAM en MiB (512..16384, defecto 2048)")),
                ("iso", prop_str("Nombre o ruta de la imagen ISO a montar")),
                ("disk_gb", prop_int("Tamaño del disco VDI dinámico en GiB (defecto 20)")),
                ("create_disk", prop_bool("Si true crea un nuevo archivo .vdi")),
                ("network", prop_str("Nombre de red virtual (defecto nat-default)")),
                ("ssh_host_port", prop_int("Puerto TCP local para redirigir al puerto 22 SSH")),
                ("ssh_user", prop_str("Usuario SSH del invitado (defecto two55)")),
                ("ssh_pass", prop_str("Contraseña SSH del invitado (defecto two55)")),
                ("unattended", prop_bool("Activar instalación desatendida OEMDRV")),
                ("use_efi", prop_bool("Usar firmware UEFI OVMF x64")),
                ("start_now", prop_bool("Arrancar la VM tras crearla")),
            ],
            vec!["name"],
        ),
        tool_schema(
            "delete_vm",
            "Elimina una máquina virtual de vms.conf (opcionalmente deteniéndola si corre y borrando su disco VDI).",
            vec![
                ("name", prop_str("Nombre de la VM a eliminar")),
                ("delete_disk", prop_bool("Si true elimina también su archivo de disco en el directorio local")),
                ("force_stop", prop_bool("Si true detiene el proceso si estaba corriendo")),
            ],
            vec!["name"],
        ),
        tool_schema(
            "configure_vm",
            "Modifica parámetros de una máquina virtual existente (CPUs, RAM, ISO, red, MAC, puerto SSH, EFI, desatendida).",
            vec![
                ("name", prop_str("Nombre actual de la VM")),
                ("new_name", prop_str("Nuevo nombre opcional")),
                ("cpus", prop_int("Nuevo número de vCPUs")),
                ("ram_mb", prop_int("Nueva RAM en MiB")),
                ("iso", prop_str("ISO a montar o 'none' para expulsar")),
                ("network", prop_str("Nombre de red virtual")),
                ("mac_address", prop_str("Dirección MAC XX:XX:XX:XX:XX:XX")),
                ("ssh_host_port", prop_int("Puerto TCP en 127.0.0.1 hacia SSH puerto 22")),
                ("ssh_user", prop_str("Usuario SSH")),
                ("ssh_pass", prop_str("Contraseña SSH")),
                ("unattended", prop_bool("Habilitar/deshabilitar instalación desatendida")),
                ("use_efi", prop_bool("Habilitar/deshabilitar EFI")),
            ],
            vec!["name"],
        ),
        tool_schema(
            "start_vm",
            "Arranca una máquina virtual existente en su ventana externa KVM.",
            vec![("name", prop_str("Nombre de la VM a iniciar"))],
            vec!["name"],
        ),
        tool_schema(
            "stop_vm",
            "Detiene una máquina virtual actualmente en ejecución.",
            vec![("name", prop_str("Nombre de la VM a detener"))],
            vec!["name"],
        ),
        tool_schema(
            "list_isos",
            "Lista todas las imágenes ISO disponibles en el sistema y su tipo de SO detectado.",
            vec![],
            vec![],
        ),
        tool_schema(
            "install_iso_auto",
            "Prepara e inicia una instalación desatendida automática de una imagen ISO con disco VDI, credenciales y puerto SSH.",
            vec![
                ("iso", prop_str("Nombre o ruta de la ISO a instalar")),
                ("vm_name", prop_str("Nombre deseado de la VM")),
                ("cpus", prop_int("Número de vCPUs (defecto 2)")),
                ("ram_mb", prop_int("Memoria RAM en MiB (defecto 2048)")),
                ("disk_gb", prop_int("Tamaño del disco VDI en GiB (defecto 20)")),
                ("username", prop_str("Usuario del sistema instalado (defecto two55)")),
                ("password", prop_str("Contraseña de usuario y root (defecto two55)")),
                ("network", prop_str("Red virtual (defecto nat-default)")),
                ("ssh_host_port", prop_int("Puerto local para acceder por SSH")),
                ("use_efi", prop_bool("Usar UEFI")),
                ("start_now", prop_bool("Iniciar automáticamente tras preparar (defecto true)")),
            ],
            vec!["iso"],
        ),
        tool_schema(
            "list_networks",
            "Lista todas las redes virtuales configuradas, su modo, subred, gateway, port-forwards y VMs conectadas.",
            vec![],
            vec![],
        ),
        tool_schema(
            "create_network",
            "Crea una nueva red virtual (nat, hostonly, internal o tap) con subred CIDR y port-forwards.",
            vec![
                ("name", prop_str("Nombre de la red virtual")),
                ("mode", prop_str("Modo: nat, hostonly, internal o tap")),
                ("subnet", prop_str("Subred CIDR (ej. 10.0.20.0/24)")),
                ("tap_if", prop_str("Nombre de interfaz TAP si mode=tap (ej. tap0)")),
                (
                    "port_forwards",
                    JsonValue::obj(vec![
                        ("type", JsonValue::Str("array".to_string())),
                        ("description", JsonValue::Str("Lista de redirecciones host:guest (ej. [\"2225:22\"])".to_string())),
                        ("items", JsonValue::obj(vec![("type", JsonValue::Str("string".to_string()))])),
                    ]),
                ),
            ],
            vec!["name"],
        ),
        tool_schema(
            "delete_network",
            "Elimina una red virtual personalizada y reasigna sus VMs a nat-default.",
            vec![("name", prop_str("Nombre de la red a eliminar"))],
            vec!["name"],
        ),
        tool_schema(
            "assign_vm_network",
            "Conecta una VM a otra red virtual y opcionalmente regenera su MAC o asigna puerto SSH.",
            vec![
                ("vm_name", prop_str("Nombre de la VM")),
                ("network_name", prop_str("Nombre de la red destino")),
                ("regenerate_mac", prop_bool("Generar nueva MAC 02:55:00")),
                ("ssh_host_port", prop_int("Puerto host para redirigir SSH (puerto 22)")),
            ],
            vec!["vm_name", "network_name"],
        ),
        tool_schema(
            "manage_port_forward",
            "Agrega o elimina una redirección de puerto TCP en una red virtual.",
            vec![
                ("action", prop_str("Acción: add o remove")),
                ("network_name", prop_str("Nombre de la red (defecto nat-default)")),
                ("host_port", prop_int("Puerto TCP en 127.0.0.1 del host")),
                ("guest_port", prop_int("Puerto TCP dentro de la VM")),
            ],
            vec!["action", "host_port", "guest_port"],
        ),
        tool_schema(
            "ssh_get_vm_info",
            "Conecta por SSH a una VM para consultar hostname, kernel, interfaces IP, memoria, disco y uptime.",
            vec![
                ("vm_name", prop_str("Nombre de la VM configurada")),
                ("host", prop_str("IP opcional (por defecto 127.0.0.1)")),
                ("port", prop_int("Puerto SSH opcional")),
                ("user", prop_str("Usuario SSH opcional")),
                ("password", prop_str("Contraseña SSH opcional")),
                ("key_path", prop_str("Ruta a llave privada SSH opcional")),
            ],
            vec![],
        ),
        tool_schema(
            "ssh_modify_vm",
            "Modifica parámetros del sistema invitado mediante SSH: set_hostname, set_sysctl, configure_network, manage_service, install_package o exec_command.",
            vec![
                ("vm_name", prop_str("Nombre de la VM")),
                ("action", prop_str("set_hostname | set_sysctl | configure_network | manage_service | install_package | exec_command")),
                ("hostname", prop_str("Nuevo hostname si action=set_hostname")),
                ("sysctl_key", prop_str("Clave sysctl (ej. net.ipv4.ip_forward)")),
                ("sysctl_value", prop_str("Valor sysctl")),
                ("persist", prop_bool("Guardar sysctl en /etc/sysctl.d/99-two55.conf")),
                ("iface", prop_str("Interfaz de red del invitado (ej. eth0, enp0s3)")),
                ("net_mode", prop_str("dhcp o static")),
                ("ip_cidr", prop_str("IP/CIDR para static (ej. 10.0.2.25/24)")),
                ("gateway", prop_str("Puerta de enlace opcional")),
                ("dns", prop_str("Servidor DNS opcional")),
                ("service", prop_str("Nombre del servicio systemd/sysv")),
                ("service_action", prop_str("start | stop | restart | reload | enable | disable | status")),
                ("package", prop_str("Paquete a instalar")),
                ("command", prop_str("Comando shell si action=exec_command")),
                ("host", prop_str("IP opcional (por defecto 127.0.0.1)")),
                ("port", prop_int("Puerto SSH opcional")),
                ("user", prop_str("Usuario SSH opcional")),
                ("password", prop_str("Contraseña SSH opcional")),
                ("key_path", prop_str("Llave privada SSH opcional")),
            ],
            vec!["action"],
        ),
    ]
}

#[derive(Debug, Clone)]
pub enum AgentUiEvent {
    StatusUpdate {
        online: bool,
        endpoint_label: String,
        is_remote: bool,
        url: String,
        models: Vec<String>,
        active_model: String,
    },
    SystemNote(String),
    Thinking(bool),
    ToolLog(String),
    AssistantReply(String),
    Error(String),
}

enum WorkerCmd {
    SendPrompt(String),
    RefreshModels,
    CycleModel,
    CycleEndpoint,
    ClearHistory,
    Shutdown,
}

/// Manejador en segundo plano del Agente IA para integrarlo con el bucle `minifb` sin bloquear la UI.
pub struct AgentWorkerHandle {
    cmd_tx: mpsc::Sender<WorkerCmd>,
    ui_rx: mpsc::Receiver<AgentUiEvent>,
    backend_rx: mpsc::Receiver<BackendEnvelope>,
}

impl AgentWorkerHandle {
    pub fn spawn() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (ui_tx, ui_rx) = mpsc::channel();
        let (backend_tx, backend_rx) = mpsc::channel();

        let _ = std::thread::Builder::new()
            .name("ollama-agent".to_string())
            .spawn(move || {
                let mut backend = ChannelBackend { req_tx: backend_tx };
                let mut client = OllamaClient::new(OllamaClient::resolve_default_url());
                let mut endpoint_label = String::from("bogo");
                let mut is_remote = true;
                let mut prefer_local = false;
                let mut model_names: Vec<String> = Vec::new();
                let mut active_model = String::new();
                let mut online = false;

                refresh_connection(
                    &mut client,
                    &mut endpoint_label,
                    &mut is_remote,
                    prefer_local,
                    &mut model_names,
                    &mut active_model,
                    &mut online,
                    &ui_tx,
                    true,
                );

                let mut history = vec![ChatMessage::system(SYSTEM_PROMPT)];
                let tools = agent_tool_schemas();

                while let Ok(cmd) = cmd_rx.recv() {
                    match cmd {
                        WorkerCmd::Shutdown => break,
                        WorkerCmd::RefreshModels => {
                            refresh_connection(
                                &mut client,
                                &mut endpoint_label,
                                &mut is_remote,
                                prefer_local,
                                &mut model_names,
                                &mut active_model,
                                &mut online,
                                &ui_tx,
                                true,
                            );
                        }
                        WorkerCmd::CycleEndpoint => {
                            prefer_local = !prefer_local;
                            refresh_connection(
                                &mut client,
                                &mut endpoint_label,
                                &mut is_remote,
                                prefer_local,
                                &mut model_names,
                                &mut active_model,
                                &mut online,
                                &ui_tx,
                                true,
                            );
                        }
                        WorkerCmd::CycleModel => {
                            if !model_names.is_empty() {
                                let cur = model_names
                                    .iter()
                                    .position(|m| *m == active_model)
                                    .unwrap_or(0);
                                let next = (cur + 1) % model_names.len();
                                active_model = model_names[next].clone();
                                let _ = ui_tx.send(AgentUiEvent::StatusUpdate {
                                    online,
                                    endpoint_label: endpoint_label.clone(),
                                    is_remote,
                                    url: client.base_url.clone(),
                                    models: model_names.clone(),
                                    active_model: active_model.clone(),
                                });
                            }
                        }
                        WorkerCmd::ClearHistory => {
                            history.truncate(1);
                        }
                        WorkerCmd::SendPrompt(prompt) => {
                            if !online || active_model.is_empty() {
                                refresh_connection(
                                    &mut client,
                                    &mut endpoint_label,
                                    &mut is_remote,
                                    prefer_local,
                                    &mut model_names,
                                    &mut active_model,
                                    &mut online,
                                    &ui_tx,
                                    true,
                                );
                            }
                            if !online || active_model.is_empty() {
                                let _ = ui_tx.send(AgentUiEvent::Error(format!(
                                    "Ollama ({}) no responde en {}. Verifica Tailscale/ollama serve y pulsa [R].",
                                    endpoint_label, client.base_url
                                )));
                                let _ = ui_tx.send(AgentUiEvent::Thinking(false));
                                continue;
                            }

                            history.push(ChatMessage::user(prompt));
                            // Mantener ventana manejable de historial
                            if history.len() > 28 {
                                let tail = history.split_off(history.len() - 24);
                                history.truncate(1);
                                history.extend(tail);
                            }

                            match run_agent_loop(
                                &client,
                                &active_model,
                                &mut history,
                                &tools,
                                &mut backend,
                                |log| {
                                    let _ = ui_tx.send(AgentUiEvent::ToolLog(log));
                                },
                            ) {
                                Ok(reply) => {
                                    let _ = ui_tx.send(AgentUiEvent::AssistantReply(reply));
                                }
                                Err(err) => {
                                    let _ = ui_tx.send(AgentUiEvent::Error(err));
                                }
                            }
                            let _ = ui_tx.send(AgentUiEvent::Thinking(false));
                        }
                    }
                }
            });

        Self {
            cmd_tx,
            ui_rx,
            backend_rx,
        }
    }

    pub fn send_prompt(&self, prompt: String) {
        let _ = self.cmd_tx.send(WorkerCmd::SendPrompt(prompt));
    }

    pub fn refresh_models(&self) {
        let _ = self.cmd_tx.send(WorkerCmd::RefreshModels);
    }

    pub fn cycle_model(&self) {
        let _ = self.cmd_tx.send(WorkerCmd::CycleModel);
    }

    pub fn cycle_endpoint(&self) {
        let _ = self.cmd_tx.send(WorkerCmd::CycleEndpoint);
    }

    pub fn clear_history(&self) {
        let _ = self.cmd_tx.send(WorkerCmd::ClearHistory);
    }

    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(WorkerCmd::Shutdown);
    }

    pub fn poll_ui_events(&self) -> Vec<AgentUiEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.ui_rx.try_recv() {
            out.push(ev);
        }
        out
    }

    pub fn poll_backend_requests(&self, st: &mut HypervisorState<'_>) -> bool {
        let mut mutated = false;
        while let Ok(env) = self.backend_rx.try_recv() {
            let res = execute_state_op(&env.op.tool_name, &env.op.args, st);
            let _ = env.reply_tx.send(res);
            mutated = true;
        }
        mutated
    }
}

#[allow(clippy::too_many_arguments)]
fn refresh_connection(
    client: &mut OllamaClient,
    endpoint_label: &mut String,
    is_remote: &mut bool,
    prefer_local: bool,
    model_names: &mut Vec<String>,
    active_model: &mut String,
    online: &mut bool,
    ui_tx: &mpsc::Sender<AgentUiEvent>,
    emit_note: bool,
) {
    if prefer_local {
        let local_client = OllamaClient::new(crate::ollama::DEFAULT_LOCAL_OLLAMA_URL);
        if let Ok(models) = local_client.list_models() {
            *client = local_client;
            *endpoint_label = "local".to_string();
            *is_remote = false;
            *model_names = models.iter().map(|m| m.name.clone()).collect();
            if active_model.is_empty() || !model_names.contains(active_model) {
                *active_model = OllamaClient::pick_best_model_for(&models, false).unwrap_or_default();
            }
            *online = !models.is_empty();
            let _ = ui_tx.send(AgentUiEvent::StatusUpdate {
                online: *online,
                endpoint_label: endpoint_label.clone(),
                is_remote: *is_remote,
                url: client.base_url.clone(),
                models: model_names.clone(),
                active_model: active_model.clone(),
            });
            if emit_note {
                let _ = ui_tx.send(AgentUiEvent::SystemNote(format!(
                    "Modo manual: Ollama local ({}).",
                    client.base_url
                )));
            }
            return;
        }
    }

    match OllamaClient::discover_endpoint_detailed(false) {
        Ok(discovered) => {
            *client = discovered.client;
            *endpoint_label = discovered.endpoint.label;
            *is_remote = discovered.endpoint.is_remote;
            *model_names = discovered.models.iter().map(|m| m.name.clone()).collect();
            if active_model.is_empty() || !model_names.contains(active_model) {
                *active_model =
                    OllamaClient::pick_best_model_for(&discovered.models, *is_remote)
                        .unwrap_or_default();
            }
            *online = !discovered.models.is_empty();
            if emit_note {
                if let Some(note) = discovered.fallback_note {
                    let _ = ui_tx.send(AgentUiEvent::SystemNote(note));
                } else if *is_remote {
                    let _ = ui_tx.send(AgentUiEvent::SystemNote(format!(
                        "Conectado a servidor GPU {} ({}) por Tailscale.",
                        endpoint_label, client.base_url
                    )));
                }
            }
        }
        Err(err) => {
            *online = false;
            model_names.clear();
            if emit_note {
                let _ = ui_tx.send(AgentUiEvent::SystemNote(format!(
                    "Sin conexion a bogo ni Ollama local: {}",
                    err
                )));
            }
        }
    }
    let _ = ui_tx.send(AgentUiEvent::StatusUpdate {
        online: *online,
        endpoint_label: endpoint_label.clone(),
        is_remote: *is_remote,
        url: client.base_url.clone(),
        models: model_names.clone(),
        active_model: active_model.clone(),
    });
}

/// Bucle de tool-calling con Ollama (hasta `MAX_TOOL_ROUNDS` rondas).
pub fn run_agent_loop<F>(
    client: &OllamaClient,
    model: &str,
    history: &mut Vec<ChatMessage>,
    tools: &[JsonValue],
    backend: &mut dyn AgentBackend,
    mut on_tool_log: F,
) -> Result<String, String>
where
    F: FnMut(String),
{
    for _round in 0..MAX_TOOL_ROUNDS {
        let resp = client.chat(model, history, tools)?;
        if resp.tool_calls.is_empty() {
            let clean = if resp.content.trim().is_empty() {
                "Operación completada.".to_string()
            } else {
                resp.content.trim().to_string()
            };
            history.push(ChatMessage::assistant(clean.clone(), Vec::new()));
            return Ok(clean);
        }

        history.push(ChatMessage::assistant(
            resp.content.clone(),
            resp.tool_calls.clone(),
        ));

        for tc in &resp.tool_calls {
            on_tool_log(format!("{}({})", tc.name, summarize_args(&tc.arguments)));
            let result_str = match dispatch_tool(backend, &tc.name, &tc.arguments) {
                Ok(ok_json) => ok_json,
                Err(err_msg) => JsonValue::obj(vec![
                    ("ok", JsonValue::Bool(false)),
                    ("error", JsonValue::Str(err_msg)),
                ])
                .to_json_string(),
            };
            history.push(ChatMessage::tool(tc.name.clone(), result_str));
        }
    }
    Ok("Se alcanzó el límite máximo de pasos del agente para esta consulta.".to_string())
}

fn summarize_args(args: &JsonValue) -> String {
    let raw = args.to_json_string();
    if raw == "{}" {
        String::new()
    } else if raw.chars().count() > 72 {
        let short: String = raw.chars().take(69).collect();
        format!("{}...", short)
    } else {
        raw
    }
}

/// Ejecuta una consulta en modo CLI sin abrir GUI (`--agent-prompt`).
pub fn run_headless_prompt(
    prompt: &str,
    model_override: Option<&str>,
    url_override: Option<&str>,
    bogo_only: bool,
) -> Result<String, String> {
    let (client, endpoint_label, is_remote, models, fallback_note) = if let Some(u) = url_override {
        let c = OllamaClient::new(u);
        let m = c.list_models()?;
        let rem = !crate::ollama::is_loopback_url(&c.base_url);
        let lbl = if c.base_url.contains("100.") || c.base_url.to_ascii_lowercase().contains("bogo") {
            "bogo".to_string()
        } else if rem {
            "custom".to_string()
        } else {
            "local".to_string()
        };
        (c, lbl, rem, m, None)
    } else {
        let d = OllamaClient::discover_endpoint_detailed(bogo_only)?;
        (
            d.client,
            d.endpoint.label,
            d.endpoint.is_remote,
            d.models,
            d.fallback_note,
        )
    };

    if let Some(note) = fallback_note {
        eprintln!("[AGENT] AVISO: {}", note);
    }

    let model = match model_override {
        Some(m) if !m.trim().is_empty() => m.trim().to_string(),
        _ => OllamaClient::pick_best_model_for(&models, is_remote)
            .ok_or_else(|| "No hay modelos descargados en Ollama".to_string())?,
    };

    eprintln!(
        "[AGENT] Servidor {} ({}) | modelo: {}",
        endpoint_label, client.base_url, model
    );
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut backend = DirectBackend::open(cwd);
    let mut history = vec![
        ChatMessage::system(SYSTEM_PROMPT),
        ChatMessage::user(prompt.to_string()),
    ];
    let tools = agent_tool_schemas();

    run_agent_loop(
        &client,
        &model,
        &mut history,
        &tools,
        &mut backend,
        |log| {
            eprintln!("[AGENT:TOOL] {}", log);
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tff_agent_test_{}_{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_agent_vm_and_network_tools_end_to_end() {
        let dir = temp_workdir("full");
        // Crear una ISO dummy local para probar install_iso_auto
        let dummy_iso = dir.join("debian-12-netinst.iso");
        let mut iso_buf = vec![0u8; 32768 + 2048];
        iso_buf[32768] = 1;
        iso_buf[32769..32774].copy_from_slice(b"CD001");
        iso_buf[32768 + 40..32768 + 49].copy_from_slice(b"DEBIAN_12");
        fs::write(&dummy_iso, iso_buf).unwrap();

        let mut backend = DirectBackend::open(dir.clone());
        backend.available_isos.push(dummy_iso.to_string_lossy().to_string());

        // 1. Crear red virtual
        let net_res = dispatch_tool(
            &mut backend,
            "create_network",
            &JsonValue::parse(r#"{"name":"red-lab","mode":"nat","subnet":"10.0.20.0/24","port_forwards":["2225:22"]}"#).unwrap(),
        )
        .expect("create_network");
        assert!(net_res.contains("red-lab"));

        // 2. Listar redes
        let list_nets = dispatch_tool(&mut backend, "list_networks", &JsonValue::obj(vec![])).unwrap();
        assert!(list_nets.contains("10.0.20.0/24"));

        // 3. Crear VM conectada a la red nueva
        let vm_res = dispatch_tool(
            &mut backend,
            "create_vm",
            &JsonValue::parse(r#"{"name":"LabBox","cpus":2,"ram_mb":2048,"disk_gb":10,"network":"red-lab","ssh_host_port":2226}"#).unwrap(),
        )
        .expect("create_vm");
        assert!(vm_res.contains("LabBox"));
        assert!(dir.join("vms.conf").exists());
        assert!(dir.join("labbox.vdi").exists());

        // 4. Configurar VM y port forward
        let cfg_res = dispatch_tool(
            &mut backend,
            "configure_vm",
            &JsonValue::parse(r#"{"name":"LabBox","cpus":4,"ram_mb":3072,"use_efi":true}"#).unwrap(),
        )
        .expect("configure_vm");
        assert!(cfg_res.contains("\"cpus\":4"));

        let pf_res = dispatch_tool(
            &mut backend,
            "manage_port_forward",
            &JsonValue::parse(r#"{"action":"add","network_name":"red-lab","host_port":8080,"guest_port":80}"#).unwrap(),
        )
        .expect("manage_port_forward");
        assert!(pf_res.contains("8080:80"));

        // 5. Instalación automática de ISO (start_now: false)
        let inst_res = dispatch_tool(
            &mut backend,
            "install_iso_auto",
            &JsonValue::parse(r#"{"iso":"debian-12-netinst.iso","vm_name":"AutoDebian","cpus":2,"ram_mb":1024,"disk_gb":8,"start_now":false}"#).unwrap(),
        )
        .expect("install_iso_auto");
        assert!(inst_res.contains("\"unattended\":true"));
        assert!(dir.join("autodebian.vdi").exists());

        // 6. Verificar validación SSH (rechaza host público)
        let ssh_bad = dispatch_tool(
            &mut backend,
            "ssh_get_vm_info",
            &JsonValue::parse(r#"{"host":"8.8.8.8","port":22}"#).unwrap(),
        );
        assert!(ssh_bad.is_err());

        // 7. Eliminar VM y su disco
        let del_res = dispatch_tool(
            &mut backend,
            "delete_vm",
            &JsonValue::parse(r#"{"name":"LabBox","delete_disk":true}"#).unwrap(),
        )
        .expect("delete_vm");
        assert!(del_res.contains("\"disk_deleted\":true"));
        assert!(!dir.join("labbox.vdi").exists());

        // 8. Eliminar red y comprobar reasignación
        let del_net = dispatch_tool(
            &mut backend,
            "delete_network",
            &JsonValue::parse(r#"{"name":"red-lab"}"#).unwrap(),
        )
        .expect("delete_network");
        assert!(del_net.contains("red-lab"));

        let _ = fs::remove_dir_all(&dir);
    }
}
