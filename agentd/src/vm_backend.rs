
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

static NEXT_SSH_PORT: AtomicU16 = AtomicU16::new(10022);

#[derive(Debug, Clone)]
pub enum VmBackend {
    Qemu,
    Firecracker, // Future: if /dev/kvm
}

#[derive(Debug, Clone)]
pub struct VmHandle {
    pub sandbox_id: String,
    pub pid: u32,
    pub backend: VmBackend,
    pub ssh_port: u16,
    pub ssh_key: PathBuf,
    pub rootfs_path: PathBuf, // /tmp/vm-{id}-rootfs.ext4
}

pub fn detect_vm_backend() -> VmBackend {
    if PathBuf::from("/dev/kvm").exists() {
        if Command::new("firecracker").status().is_ok() {
            return VmBackend::Firecracker;
        }
    }
    VmBackend::Qemu // Codespace default
}

pub fn boot_vm(sandbox_id: String, host_root: &std::path::Path, image_hint: &str) -> Result<VmHandle> {
    let assets = dirs::home_dir().unwrap().join(".mowis/vm-assets");
    let kernel = assets.join("vmlinux");
    let mut rootfs_img = assets.join(format!("sandbox-{}.ext4", sandbox_id));
    
    // Copy host_root to VM rootfs (overlayfs upper → ext4 for VM drive)
    if !rootfs_img.exists() {
        fs::create_dir_all(rootfs_img.parent().unwrap())?;
        Command::new("cp")
            .args([&rootfs_img, &assets.join("mowis-rootfs.ext4")])
            .status()?;
        
        // Mount, copy host packages/workspace, inject SSH pubkey
        let mountpt = tempfile::tempdir()?;
        Command::new("sudo").args(["mount", "-o", "loop", rootfs_img.to_str().unwrap(), mountpt.path().to_str().unwrap()]).status()?;
        
        // Copy /workspace from host_root
        if host_root.join("workspace").exists() {
            copy_dir_all(host_root.join("workspace"), mountpt.path().join("workspace"))?;
        }
        
        // SSH setup (pubkey injection)
        let keypair = generate_ssh_keypair(&sandbox_id)?;
        let pubkey = fs::read_to_string(&keypair.1)?;
        fs::create_dir_all(mountpt.path().join("root/.ssh"))?;
        fs::write(mountpt.path().join("root/.ssh/authorized_keys"), pubkey)?;
        fs::set_permissions(mountpt.path().join("root/.ssh"), fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(mountpt.path().join("root/.ssh/authorized_keys"), fs::Permissions::from_mode(0o600))?;
        
        Command::new("sudo").args(["umount", mountpt.path().to_str().unwrap()]).status()?;
    }
    
    let ssh_port = NEXT_SSH_PORT.fetch_add(1, Ordering::SeqCst);
    let child = Command::new("qemu-system-x86_64")
        .args([
            "-kernel", kernel.to_str().unwrap(),
            "-drive", &format!("file={},format=raw,if=virtio", rootfs_img.display()),
            "-append", "console=ttyS0 root=/dev/vda rw init=/init",
            "-m", "256M",
            "-smp", "1",
            "-nographic",
            "-no-reboot",
            "-net", &format!("user,hostfwd=tcp::{}-:22", ssh_port),
            "-net", "nic,model=virtio-net-pci",
            "-serial", "stdio", // For MOWIS_READY
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("qemu spawn")?;
    
    let pid = child.id();
    
    // Wait for MOWIS_READY with SSH retry (Codespace TCG ~2-5s boot)
    let handle = VmHandle {
        sandbox_id,
        pid,
        backend: VmBackend::Qemu,
        ssh_port,
        ssh_key: keypair.0,
        rootfs_path: rootfs_img,
    };
    
    if wait_vm_ready(&handle, Duration::from_secs(30))? {
        println!("[vm_backend] QEMU VM ready sandbox={} port={}", sandbox_id, ssh_port);
        Ok(handle)
    } else {
        Err(anyhow!("VM boot timeout sandbox={}", sandbox_id))
    }
}

fn wait_vm_ready(handle: &VmHandle, timeout: Duration) -> Result<bool> {
    let start = Instant::now();
    loop {
        if start.elapsed() > timeout {
            return Ok(false);
        }
        // Poll SSH: ssh -o ConnectTimeout=1 should succeed
        let status = Command::new("ssh")
            .args([
                "-o", "StrictHostKeyChecking=no",
                "-o", "UserKnownHostsFile=/dev/null",
                "-o", "ConnectTimeout=1",
                "-i", handle.ssh_key.to_str().unwrap(),
                &format!("root@localhost -p {}", handle.ssh_port),
                "echo OK",
            ])
            .status();
        if status.is_ok() {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

pub fn stop_vm(handle: &VmHandle) -> Result<()> {
    // QEMU: qemu-monitor or kill
    let _ = Command::new("kill").arg(format!("{}", handle.pid)).status();
    // Cleanup
    let _ = fs::remove_file(&handle.rootfs_path);
    let _ = fs::remove_dir_all(handle.ssh_key.parent().unwrap());
    println!("[vm_backend] VM stopped sandbox={}", handle.sandbox_id);
    Ok(())
}

pub fn exec_in_vm(handle: &VmHandle, tool_name: &str, input: Value) -> Result<Value> {
    exec_in_vm_ssh(handle, tool_name, input)
}

fn exec_in_vm_ssh(handle: &VmHandle, tool_name: &str, input: Value) -> Result<Value> {
    let cmd = map_tool_to_ssh(tool_name, input);
    let output = ssh_exec(handle, &cmd)?;
    
    // Parse output to ToolResult format
    Ok(json!({
        "success": output.status.success(),
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr)
    }))
}

fn map_tool_to_ssh(tool: &str, input: Value) -> String {
    match tool {
        "read_file" => {
            let path = input["path"].as_str().unwrap_or("");
            format!("cat /workspace/{}", path)
        }
        "write_file" => {
            let path = input["path"].as_str().unwrap_or("");
            let content_b64 = input["content"].as_str().unwrap_or("");
            format!("echo '{}' | base64 -d > /workspace/{}", content_b64, path)
        }
        "run
