//! Módulo de plantillas desatendidas (unattended) para Two Five Five (255).
//!
//! Genera configuraciones para instalaciones automatizadas:
//! - Preseed (`preseed.cfg`) para Debian y Kali Linux (`debian-installer`).
//! - Cloud-Init (`user-data` y `meta-data`) para Ubuntu Cloud y distribuciones cloud modernas.
//! - Autounattend (`autounattend.xml`) para Microsoft Windows Setup.

/// Opciones de configuración del usuario huésped para instalaciones desatendidas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnattendedConfig {
    /// Nombre del usuario normal con privilegios sudo / administrador.
    pub username: String,
    /// Contraseña tanto para el usuario normal como para root / Administrador.
    pub password: String,
    /// Nombre de host (hostname) de la máquina virtual.
    pub hostname: String,
    /// Zona horaria (ej. "UTC", "Europe/Madrid", "America/Mexico_City").
    pub timezone: String,
}

impl UnattendedConfig {
    /// Crea una nueva configuración desatendida.
    pub fn new(
        username: impl Into<String>,
        password: impl Into<String>,
        hostname: impl Into<String>,
        timezone: impl Into<String>,
    ) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
            hostname: hostname.into(),
            timezone: timezone.into(),
        }
    }
}

impl Default for UnattendedConfig {
    fn default() -> Self {
        Self {
            username: "two55".to_string(),
            password: "two55".to_string(),
            hostname: "two55-vm".to_string(),
            timezone: "UTC".to_string(),
        }
    }
}

/// Genera el contenido de un archivo `preseed.cfg` completo y compatible con `debian-installer`
/// para instalaciones 100% desatendidas de Debian y Kali Linux.
pub fn generate_debian_preseed(config: &UnattendedConfig) -> String {
    let tz_lower = config.timezone.to_lowercase();
    let is_spanish = tz_lower.contains("madrid")
        || tz_lower.contains("mexico")
        || tz_lower.contains("buenos_aires")
        || tz_lower.contains("bogota")
        || tz_lower.contains("santiago")
        || tz_lower.contains("lima")
        || tz_lower.contains("caracas")
        || tz_lower.contains("habana")
        || tz_lower.contains("montevideo")
        || tz_lower.contains("asuncion")
        || tz_lower.contains("la_paz")
        || tz_lower.contains("quito")
        || tz_lower.starts_with("es");

    let (locale, keymap, country, language) = if is_spanish {
        ("es_ES.UTF-8", "es", "ES", "es")
    } else {
        ("en_US.UTF-8", "us", "US", "en")
    };

    let username = &config.username;
    let password = &config.password;
    let hostname = &config.hostname;
    let timezone = &config.timezone;

    format!(
        r#"#_preseed_V1
#### 1. Idioma, pais y teclado (No interactivo)
d-i debian-installer/locale string {locale}
d-i debian-installer/language string {language}
d-i debian-installer/country string {country}
d-i localechooser/supported-locales multiselect en_US.UTF-8, es_ES.UTF-8
d-i console-setup/ask_detect boolean false
d-i keyboard-configuration/xkb-keymap select {keymap}

#### 2. Configuracion de red
d-i netcfg/choose_interface select auto
d-i netcfg/get_hostname string {hostname}
d-i netcfg/get_domain string local
d-i netcfg/wireless_wep string

#### 3. Configuracion de espejos / repositorios y simple-cdd
d-i simple-cdd/profiles multiselect kali, offline
d-i mirror/country string manual
d-i mirror/http/directory string /debian
d-i mirror/http/proxy string
d-i apt-setup/use_mirror boolean false
d-i apt-setup/cdrom/set-first boolean false
d-i apt-setup/cdrom/set-next boolean false
d-i apt-setup/cdrom/set-failed boolean false
d-i apt-setup/services-select multiselect
d-i apt-setup/non-free boolean true
d-i apt-setup/non-free-firmware boolean true
d-i apt-setup/contrib boolean true
d-i apt-setup/disable-cdrom-entries boolean false

#### 4. Reloj y zona horaria
d-i clock-setup/utc boolean true
d-i time/zone string {timezone}
d-i clock-setup/ntp boolean true

#### 5. Cuentas de usuario (Root y usuario normal con sudo)
# Contraseña de root
d-i passwd/root-login boolean true
d-i passwd/root-password password {password}
d-i passwd/root-password-again password {password}

# Creacion de usuario estandar y asignacion al grupo sudo
d-i passwd/make-user boolean true
d-i passwd/user-fullname string {username}
d-i passwd/username string {username}
d-i passwd/user-password password {password}
d-i passwd/user-password-again password {password}
d-i passwd/user-default-groups string audio cdrom video sudo adm

#### 6. Particionamiento de disco no interactivo
d-i partman-auto/disk string /dev/sda
d-i partman-auto/method string regular
d-i partman-auto/choose_recipe select atomic
d-i partman-partitioning/confirm_write_new_label boolean true
d-i partman-partitioning/confirm_new_label boolean true
d-i partman-partitioning/confirm_resize boolean true
d-i partman/choose_partition select finish
d-i partman/confirm boolean true
d-i partman/confirm_nooverwrite boolean true
d-i partman-basicfilesystems/no_swap boolean false
d-i partman-lvm/device_remove_lvm boolean true
d-i partman-lvm/confirm boolean true
d-i partman-lvm/confirm_nochanges boolean true
d-i partman-md/device_remove_md boolean true
d-i partman-md/confirm boolean true
d-i partman-md/confirm_nochanges boolean true
d-i partman/confirm_write_new_label boolean true

#### 7. Seleccion e instalacion de paquetes del sistema base
d-i preseed/early_command string anna-install eatmydata-udeb
tasksel tasksel/first multiselect standard, ssh-server
d-i pkgsel/include string sudo openssh-server curl
d-i pkgsel/upgrade select none
d-i pkgsel/update-policy select none
popularity-contest popularity-contest/participate boolean false

#### 7.1 Preguntas de paquetes especificos de Debian y Kali
encfs encfs/security-information boolean true
encfs encfs/security-information seen true
samba-common samba-common/dhcp boolean false
macchanger macchanger/automatically_run boolean false
wireshark-common wireshark-common/install-setuid boolean true
kismet-capture-common kismet-capture-common/install-users string
kismet-capture-common kismet-capture-common/install-setuid boolean true
sslh sslh/inetd_or_standalone select standalone
atftpd atftpd/use_inetd boolean false
tripwire tripwire/installed boolean true
tripwire tripwire/installed seen true
tripwire tripwire/rebuild-config boolean false
tripwire tripwire/rebuild-policy boolean false
tripwire tripwire/use-localkey boolean false
tripwire tripwire/use-sitekey boolean false

#### 8. Instalacion automatica del cargador de arranque GRUB en MBR (/dev/sda)
d-i grub-installer/only_debian boolean true
d-i grub-installer/with_other_os boolean true
d-i grub-installer/bootdev string /dev/sda

#### 9. Finalizacion de la instalacion y reinicio automatico
d-i finish-install/reboot_in_progress note
"#
    )
}

/// Genera el par `(user-data, meta-data)` para `cloud-init` / NoCloud (usado en Ubuntu Cloud y similares).
///
/// - `user-data`: Configuración YAML con directiva `#cloud-config`, creación de usuario con `sudo`,
///   claves en `chpasswd`, zona horaria y paquetes base.
/// - `meta-data`: Metadatos de la instancia conteniendo `instance-id` y `local-hostname`.
pub fn generate_cloud_init(config: &UnattendedConfig) -> (String, String) {
    let username = &config.username;
    let password = &config.password;
    let hostname = &config.hostname;
    let timezone = &config.timezone;

    let user_data = format!(
        r#"#cloud-config
hostname: {hostname}
fqdn: {hostname}.local
manage_etc_hosts: true

users:
  - name: {username}
    gecos: {username}
    sudo: ALL=(ALL) NOPASSWD:ALL
    groups: sudo, adm
    shell: /bin/bash
    lock_passwd: false

chpasswd:
  list: |
    {username}:{password}
    root:{password}
  expire: false

timezone: {timezone}

package_update: false
package_upgrade: false
packages:
  - sudo
  - curl
  - openssh-server

ssh_pwauth: true
disable_root: false

growpart:
  mode: auto
  devices: ['/']
  ignore_growpart: false

final_message: "Two-Five-Five cloud-init setup completed after $UPTIME seconds"
"#
    );

    let meta_data = format!(
        r#"instance-id: i-two55-{hostname}
local-hostname: {hostname}
"#
    );

    (user_data, meta_data)
}

/// Genera un archivo XML `autounattend.xml` para instalación automatizada de Microsoft Windows.
pub fn generate_windows_unattend(config: &UnattendedConfig) -> String {
    let username = &config.username;
    let password = &config.password;
    let hostname = &config.hostname;
    let timezone = &config.timezone;

    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<unattend xmlns="urn:schemas-microsoft-com:unattend">
  <settings pass="windowsPE">
    <component name="Microsoft-Windows-International-Core-WinPE" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <SetupUILanguage>
        <UILanguage>en-US</UILanguage>
      </SetupUILanguage>
      <InputLocale>en-US</InputLocale>
      <SystemLocale>en-US</SystemLocale>
      <UILanguage>en-US</UILanguage>
      <UserLocale>en-US</UserLocale>
    </component>
    <component name="Microsoft-Windows-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <DiskConfiguration>
        <Disk wcm:action="add" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
          <DiskID>0</DiskID>
          <WillWipeDisk>true</WillWipeDisk>
          <CreatePartitions>
            <CreatePartition wcm:action="add">
              <Order>1</Order>
              <Type>Primary</Type>
              <Extend>true</Extend>
            </CreatePartition>
          </CreatePartitions>
          <ModifyPartitions>
            <ModifyPartition wcm:action="add">
              <Order>1</Order>
              <PartitionID>1</PartitionID>
              <Format>NTFS</Format>
              <Label>Windows</Label>
              <Letter>C</Letter>
            </ModifyPartition>
          </ModifyPartitions>
        </Disk>
      </DiskConfiguration>
      <ImageInstall>
        <OSImage>
          <InstallTo>
            <DiskID>0</DiskID>
            <PartitionID>1</PartitionID>
          </InstallTo>
          <WillShowUI>OnError</WillShowUI>
        </OSImage>
      </ImageInstall>
      <UserData>
        <AcceptEula>true</AcceptEula>
        <FullName>{username}</FullName>
        <Organization>Two Five Five</Organization>
      </UserData>
    </component>
  </settings>
  <settings pass="specialize">
    <component name="Microsoft-Windows-Shell-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <ComputerName>{hostname}</ComputerName>
      <TimeZone>{timezone}</TimeZone>
    </component>
  </settings>
  <settings pass="oobeSystem">
    <component name="Microsoft-Windows-Shell-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <OOBE>
        <HideEULAPage>true</HideEULAPage>
        <HideLocalAccountScreen>true</HideLocalAccountScreen>
        <HideOEMRegistrationScreens>true</HideOEMRegistrationScreens>
        <HideOnlineAccountScreens>true</HideOnlineAccountScreens>
        <HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE>
        <NetworkLocation>Work</NetworkLocation>
        <ProtectYourPC>3</ProtectYourPC>
      </OOBE>
      <UserAccounts>
        <AdministratorPassword>
          <Value>{password}</Value>
          <PlainText>true</PlainText>
        </AdministratorPassword>
        <LocalAccounts>
          <LocalAccount wcm:action="add" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
            <Name>{username}</Name>
            <Group>Administrators</Group>
            <Password>
              <Value>{password}</Value>
              <PlainText>true</PlainText>
            </Password>
          </LocalAccount>
        </LocalAccounts>
      </UserAccounts>
      <AutoLogon>
        <Username>{username}</Username>
        <Password>
          <Value>{password}</Value>
          <PlainText>true</PlainText>
        </Password>
        <Enabled>true</Enabled>
        <LogonCount>1</LogonCount>
      </AutoLogon>
    </component>
  </settings>
</unattend>
"#
    )
}

/// Genera un archivo Kickstart (`ks.cfg`) para instalación desatendida en distribuciones
/// Red Hat, CentOS, Fedora, AlmaLinux y Rocky Linux (Anaconda).
pub fn generate_kickstart(config: &UnattendedConfig) -> String {
    let username = &config.username;
    let password = &config.password;
    let hostname = &config.hostname;
    let timezone = &config.timezone;

    format!(
        r#"# Kickstart auto-install for Red Hat / CentOS / Fedora / Rocky / Alma
text
lang en_US.UTF-8
keyboard us
timezone {timezone} --isUtc

network --bootproto=dhcp --device=link --activate --hostname={hostname}

rootpw --plaintext {password}
user --name={username} --password={password} --plaintext --groups=wheel

zerombr
clearpart --all --initlabel
autopart --type=plain

reboot

%packages
@core
sudo
curl
openssh-server
%end
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unattended_config_defaults() {
        let config = UnattendedConfig::default();
        assert_eq!(config.username, "two55");
        assert_eq!(config.password, "two55");
        assert_eq!(config.hostname, "two55-vm");
        assert_eq!(config.timezone, "UTC");
    }

    #[test]
    fn test_generate_debian_preseed_standard() {
        let config = UnattendedConfig::new("myuser", "secretpass123", "testhost", "UTC");
        let preseed = generate_debian_preseed(&config);

        // Verificaciones de idioma y teclado por defecto
        assert!(preseed.contains("d-i debian-installer/locale string en_US.UTF-8"));
        assert!(preseed.contains("d-i keyboard-configuration/xkb-keymap select us"));

        // Verificaciones de red y hostname
        assert!(preseed.contains("d-i netcfg/get_hostname string testhost"));
        assert!(preseed.contains("d-i netcfg/get_domain string local"));

        // Verificaciones de usuario y root
        assert!(preseed.contains("d-i passwd/root-login boolean true"));
        assert!(preseed.contains("d-i passwd/root-password password secretpass123"));
        assert!(preseed.contains("d-i passwd/username string myuser"));
        assert!(preseed.contains("d-i passwd/user-password password secretpass123"));
        assert!(preseed.contains("d-i passwd/user-default-groups string audio cdrom video sudo adm"));

        // Verificaciones de particionado automatico no interactivo
        assert!(preseed.contains("d-i partman-auto/disk string /dev/sda"));
        assert!(preseed.contains("d-i partman-auto/method string regular"));
        assert!(preseed.contains("d-i partman-auto/choose_recipe select atomic"));
        assert!(preseed.contains("d-i partman/confirm boolean true"));
        assert!(preseed.contains("d-i partman/confirm_nooverwrite boolean true"));
        assert!(preseed.contains("d-i partman-partitioning/confirm_write_new_label boolean true"));

        // Verificaciones de instalacion de paquetes base
        assert!(preseed.contains("tasksel tasksel/first multiselect standard, ssh-server"));
        assert!(preseed.contains("popularity-contest popularity-contest/participate boolean false"));

        // Verificaciones de GRUB en MBR
        assert!(preseed.contains("d-i grub-installer/only_debian boolean true"));
        assert!(preseed.contains("d-i grub-installer/bootdev string /dev/sda"));

        // Verificacion de reinicio automatico
        assert!(preseed.contains("d-i finish-install/reboot_in_progress note"));
    }

    #[test]
    fn test_generate_debian_preseed_spanish_timezone() {
        let config = UnattendedConfig::new("admin", "pass", "servidor-es", "Europe/Madrid");
        let preseed = generate_debian_preseed(&config);

        assert!(preseed.contains("d-i debian-installer/locale string es_ES.UTF-8"));
        assert!(preseed.contains("d-i keyboard-configuration/xkb-keymap select es"));
        assert!(preseed.contains("d-i time/zone string Europe/Madrid"));
    }

    #[test]
    fn test_generate_cloud_init() {
        let config = UnattendedConfig::new("clouduser", "cloudpass456", "cloud-vm", "UTC");
        let (user_data, meta_data) = generate_cloud_init(&config);

        // Verificacion de user-data
        assert!(user_data.starts_with("#cloud-config"));
        assert!(user_data.contains("hostname: cloud-vm"));
        assert!(user_data.contains("name: clouduser"));
        assert!(user_data.contains("clouduser:cloudpass456"));
        assert!(user_data.contains("root:cloudpass456"));
        assert!(user_data.contains("timezone: UTC"));
        assert!(user_data.contains("ssh_pwauth: true"));
        assert!(user_data.contains("sudo: ALL=(ALL) NOPASSWD:ALL"));

        // Verificacion de meta-data
        assert!(meta_data.contains("instance-id: i-two55-cloud-vm"));
        assert!(meta_data.contains("local-hostname: cloud-vm"));
    }

    #[test]
    fn test_generate_windows_unattend() {
        let config = UnattendedConfig::new("winadmin", "P@ssw0rd123!", "winhost", "UTC");
        let xml = generate_windows_unattend(&config);

        assert!(xml.contains("<?xml version=\"1.0\" encoding=\"utf-8\"?>"));
        assert!(xml.contains("<unattend xmlns=\"urn:schemas-microsoft-com:unattend\">"));
        assert!(xml.contains("<DiskID>0</DiskID>"));
        assert!(xml.contains("<WillWipeDisk>true</WillWipeDisk>"));
        assert!(xml.contains("<ComputerName>winhost</ComputerName>"));
        assert!(xml.contains("<Name>winadmin</Name>"));
        assert!(xml.contains("<Value>P@ssw0rd123!</Value>"));
        assert!(xml.contains("<Group>Administrators</Group>"));
        assert!(xml.contains("<AcceptEula>true</AcceptEula>"));
    }

    #[test]
    fn test_generate_kickstart() {
        let config = UnattendedConfig::new("rhadmin", "rhpass789", "centos-box", "UTC");
        let ks = generate_kickstart(&config);

        assert!(ks.contains("lang en_US.UTF-8"));
        assert!(ks.contains("keyboard us"));
        assert!(ks.contains("timezone UTC --isUtc"));
        assert!(ks.contains("network --bootproto=dhcp --device=link --activate --hostname=centos-box"));
        assert!(ks.contains("rootpw --plaintext rhpass789"));
        assert!(ks.contains("user --name=rhadmin --password=rhpass789 --plaintext --groups=wheel"));
        assert!(ks.contains("reboot"));
        assert!(ks.contains("%packages"));
    }
}
