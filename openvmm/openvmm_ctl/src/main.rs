// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `openvmm-ctl` -- a small ttrpc client for talking to an `openvmm` process
//! that was launched with the `--ttrpc-management <path>` flag.
//!
//! The intent is to support live "supplemental" operations against a VM whose
//! initial topology was configured from the openvmm command line: pause,
//! resume, and hot add/remove of NVMe namespaces on PCIe NVMe controllers
//! that were declared at launch time.

use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use inspect::Node;
use inspect::Value;
use inspect::ValueKind;
use openvmm_ttrpc_vmservice as vmservice;
use pal_async::DefaultPool;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "openvmm-ctl",
    about = "Control an openvmm instance over the --ttrpc-management socket."
)]
struct Args {
    /// Path to the openvmm management ttrpc socket (matches the value passed
    /// to `--ttrpc-management` when launching openvmm).
    #[arg(long, short = 's', global = true, default_value = "openvmm.ttrpc.sock")]
    socket: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Format {
    /// Multi-line indented tree (default; same as the openvmm REPL `x` cmd
    /// with no args).
    Tree,
    /// Dense single-line `{a: 1, b: {c: 2}}` form (the inspect crate's
    /// non-alternate Display).
    Compact,
    /// JSON encoding suitable for piping to jq.
    Json,
}

#[derive(Subcommand)]
enum Cmd {
    /// Pause the VM (equivalent to PauseVm RPC).
    Pause,

    /// Resume the VM after a pause.
    Resume,

    /// Hot add an NVMe namespace to a PCIe NVMe controller that was created at
    /// VM launch via `--nvme-pci id=<controller>,...`.
    AddNs {
        /// Controller name as supplied via `id=` on the `--nvme-pci` argument.
        #[arg(long)]
        controller: String,
        /// NVMe namespace id (1..N). Must not already be in use.
        #[arg(long)]
        nsid: u32,
        /// Host file backing the namespace. Raw image or VHD/VHDX.
        #[arg(long)]
        file: PathBuf,
        /// Open the backing file read-only.
        #[arg(long, default_value_t = false)]
        read_only: bool,
    },

    /// Hot remove an NVMe namespace previously added with `add-ns`.
    RemoveNs {
        #[arg(long)]
        controller: String,
        #[arg(long)]
        nsid: u32,
    },

    /// Inspect the live VM device tree. Equivalent to the openvmm REPL `x`
    /// command. Path defaults to "" (root); depth defaults to walking
    /// everything.
    Inspect {
        /// Inspect path. Examples: "", "/", "vm", "chipset/nvme:rp0",
        /// "chipset/nvme:rp0/namespaces".
        #[arg(default_value = "")]
        path: String,
        /// Tree depth to expand. 0 = leaves only; unbounded if omitted.
        #[arg(long, short)]
        depth: Option<u32>,
        /// Output format.
        #[arg(long, short = 'f', value_enum, default_value_t = Format::Tree)]
        format: Format,
    },

    /// Pretty-printed summary of NVMe controllers and namespaces in the live
    /// VM. Backed by a single Inspect("vm", depth=10) RPC; this is the
    /// preferred at-a-glance view.
    Status,

    /// Hot add a VMBus SCSI disk to the default storvsp controller. The VM
    /// must have a SCSI controller (true for nearly all openvmm configs --
    /// see `Vm.scsi_rpc` in ttrpc/mod.rs).
    AddScsiDisk {
        /// Target LUN. Must not already be in use.
        #[arg(long)]
        lun: u32,
        /// Host file backing the disk. Raw image or VHD/VHDX (auto-detected
        /// by `open_disk_type` server-side; the `DiskType` field in the
        /// proto is ignored).
        #[arg(long)]
        file: PathBuf,
        /// Open the backing file read-only.
        #[arg(long, default_value_t = false)]
        read_only: bool,
    },

    /// Hot remove a VMBus SCSI disk by LUN.
    RemoveScsiDisk {
        #[arg(long)]
        lun: u32,
    },

    /// Save a snapshot of the VM (memory + state) to the given directory and
    /// leave the VM paused. Requires openvmm to have been launched with
    /// file-backed memory (e.g. `--memory file=<path>`). After save the VM
    /// is paused; to resume, terminate openvmm and relaunch with
    /// `--restore-snapshot <dir>`.
    SaveSnapshot {
        /// Target directory. Must not already exist.
        #[arg(long)]
        dir: PathBuf,
    },

    /// Synthetic platform reset on the running VM. Re-initializes all
    /// devices and re-runs UEFI / firmware boot. Equivalent to pressing
    /// a hard reset button: guest disk state is preserved, in-memory state
    /// is discarded. Use this to bring a halted or stuck VM back to life
    /// without killing openvmm.
    Reset,

    /// Clear the halted flag on a guest that previously powered off but
    /// whose openvmm process is still alive. Lighter than `reset` -- does
    /// not reinitialize devices. After ClearHalt the BSP can run again; if
    /// you want a clean boot, use `reset` instead.
    ClearHalt,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    DefaultPool::run_with(async |driver| {
        let socket_path = args.socket.clone();
        let client = mesh_rpc::Client::new(
            &driver,
            mesh_rpc::client::UnixDialier::new(driver.clone(), socket_path),
        );

        match args.cmd {
            Cmd::Pause => {
                client
                    .call()
                    .start(vmservice::Vm::PauseVm, ())
                    .await
                    .map_err(|s| anyhow::anyhow!("PauseVm rpc failed: {s:?}"))?;
                println!("paused");
            }
            Cmd::Resume => {
                client
                    .call()
                    .start(vmservice::Vm::ResumeVm, ())
                    .await
                    .map_err(|s| anyhow::anyhow!("ResumeVm rpc failed: {s:?}"))?;
                println!("resumed");
            }
            Cmd::AddNs {
                controller,
                nsid,
                file,
                read_only,
            } => {
                let req = vmservice::ModifyResourceRequest {
                    r#type: vmservice::ModifyType::Add.into(),
                    resource: Some(
                        vmservice::modify_resource_request::Resource::NvmeNamespace(
                            vmservice::NvmeNamespaceConfig {
                                controller_name: controller,
                                nsid,
                                host_path: file.to_string_lossy().into_owned(),
                                read_only,
                            },
                        ),
                    ),
                };
                client
                    .call()
                    .start(vmservice::Vm::ModifyResource, req)
                    .await
                    .map_err(|s| anyhow::anyhow!("ModifyResource(Add) rpc failed: {s:?}"))?;
                println!("namespace added");
            }
            Cmd::RemoveNs { controller, nsid } => {
                let req = vmservice::ModifyResourceRequest {
                    r#type: vmservice::ModifyType::Remove.into(),
                    resource: Some(
                        vmservice::modify_resource_request::Resource::NvmeNamespace(
                            vmservice::NvmeNamespaceConfig {
                                controller_name: controller,
                                nsid,
                                host_path: String::new(),
                                read_only: false,
                            },
                        ),
                    ),
                };
                client
                    .call()
                    .start(vmservice::Vm::ModifyResource, req)
                    .await
                    .map_err(|s| anyhow::anyhow!("ModifyResource(Remove) rpc failed: {s:?}"))?;
                println!("namespace removed");
            }
            Cmd::Inspect { path, depth, format } => {
                let resp = client
                    .call()
                    .start(
                        inspect_proto::InspectService::Inspect,
                        inspect_proto::InspectRequest {
                            path: path.clone(),
                            depth: depth.unwrap_or(u32::MAX),
                        },
                    )
                    .await
                    .map_err(|s| anyhow::anyhow!("Inspect rpc failed: {s:?}"))?;
                match format {
                    Format::Tree => println!("{:#}", resp.result),
                    Format::Compact => println!("{}", resp.result),
                    Format::Json => println!("{}", resp.result.json()),
                }
            }
            Cmd::Status => {
                let resp = client
                    .call()
                    .start(
                        inspect_proto::InspectService::Inspect,
                        inspect_proto::InspectRequest {
                            path: "vm".to_string(),
                            depth: 10,
                        },
                    )
                    .await
                    .map_err(|s| anyhow::anyhow!("Inspect(vm) rpc failed: {s:?}"))?;
                print_status(&resp.result);
            }
            Cmd::AddScsiDisk {
                lun,
                file,
                read_only,
            } => {
                let req = vmservice::ModifyResourceRequest {
                    r#type: vmservice::ModifyType::Add.into(),
                    resource: Some(vmservice::modify_resource_request::Resource::ScsiDisk(
                        vmservice::ScsiDisk {
                            controller: 0,
                            lun,
                            host_path: file.to_string_lossy().into_owned(),
                            r#type: vmservice::DiskType::ScsiDiskTypeVhdx.into(),
                            read_only,
                        },
                    )),
                };
                client
                    .call()
                    .start(vmservice::Vm::ModifyResource, req)
                    .await
                    .map_err(|s| anyhow::anyhow!("ModifyResource(Add SCSI) rpc failed: {s:?}"))?;
                println!("scsi disk added at lun {lun}");
            }
            Cmd::RemoveScsiDisk { lun } => {
                let req = vmservice::ModifyResourceRequest {
                    r#type: vmservice::ModifyType::Remove.into(),
                    resource: Some(vmservice::modify_resource_request::Resource::ScsiDisk(
                        vmservice::ScsiDisk {
                            controller: 0,
                            lun,
                            host_path: String::new(),
                            r#type: vmservice::DiskType::ScsiDiskTypeVhdx.into(),
                            read_only: false,
                        },
                    )),
                };
                client
                    .call()
                    .start(vmservice::Vm::ModifyResource, req)
                    .await
                    .map_err(|s| anyhow::anyhow!("ModifyResource(Remove SCSI) rpc failed: {s:?}"))?;
                println!("scsi disk removed at lun {lun}");
            }
            Cmd::SaveSnapshot { dir } => {
                let req = vmservice::SaveSnapshotRequest {
                    dir: dir.to_string_lossy().into_owned(),
                };
                client
                    .call()
                    .start(vmservice::Vm::SaveSnapshot, req)
                    .await
                    .map_err(|s| anyhow::anyhow!("SaveSnapshot rpc failed: {s:?}"))?;
                println!(
                    "snapshot saved to {}; VM is paused. Stop openvmm and relaunch with --restore-snapshot <dir> to continue.",
                    dir.display()
                );
            }
            Cmd::Reset => {
                client
                    .call()
                    .start(vmservice::Vm::ResetVm, ())
                    .await
                    .map_err(|s| anyhow::anyhow!("ResetVm rpc failed: {s:?}"))?;
                println!("reset issued; guest is rebooting");
            }
            Cmd::ClearHalt => {
                client
                    .call()
                    .start(vmservice::Vm::ClearHaltVm, ())
                    .await
                    .map_err(|s| anyhow::anyhow!("ClearHaltVm rpc failed: {s:?}"))?;
                println!("halt cleared");
            }
        }

        anyhow::Ok(())
    })?;

    Ok(())
}

// =========================================================================
// "status" rendering -- walks the Node tree returned for path="vm" and
// pretty-prints the NVMe topology in a diskdiag-style layout.
// =========================================================================

fn print_status(vm: &Node) {
    let entries = match vm {
        Node::Dir(e) => e,
        other => {
            println!("(VM root not a directory: {other:#})");
            return;
        }
    };

    // VM-wide power state up front. This is the authoritative answer to
    // "is the guest doing anything right now?" -- it reflects what the
    // virtual CPUs are doing. Per-controller `unit_state` only reflects
    // whether the device's worker task is alive in openvmm, NOT whether
    // it's actively handling IO; for that, look here first.
    let power_state =
        find_string(entries, &["partition", "power_state"]).unwrap_or_else(|| "(unknown)".into());
    let halt_count = find_u64(entries, &["partition", "halt_count"]).unwrap_or(0);
    let vm_halted = power_state == "halted";
    if halt_count > 0 || vm_halted {
        println!("VM: power_state={power_state}  halt_count={halt_count}");
    } else {
        println!("VM: power_state={power_state}");
    }

    // Iterate top-level entries, looking for NVMe controllers. They appear
    // as `pcie:<name>-nvme`.
    let mut controllers: Vec<(&str, &Node)> = Vec::new();
    for e in entries {
        if let Some(name) = e.name.strip_prefix("pcie:") {
            if let Some(ctrl_name) = name.strip_suffix("-nvme") {
                controllers.push((ctrl_name, &e.node));
            }
        }
    }

    if controllers.is_empty() {
        println!("  (no NVMe controllers found)");
        return;
    }

    for (i, (name, node)) in controllers.iter().enumerate() {
        if i > 0 {
            println!();
        }
        print_controller(name, node, vm_halted);
    }
}

fn print_controller(name: &str, node: &Node, vm_halted: bool) {
    let dir = match node {
        Node::Dir(d) => d,
        _ => {
            println!("Controller {name}: <unevaluated>");
            return;
        }
    };

    let vid = find_u64(dir, &["cfg_space", "hardware_ids", "vendor_id"]);
    let did = find_u64(dir, &["cfg_space", "hardware_ids", "device_id"]);
    let subsys = find_string(dir, &["config", "subsystem_id"]).unwrap_or_default();
    let unit_state =
        find_string(dir, &["unit_state"]).unwrap_or_else(|| "(unknown)".to_string());
    let bar0 = find_string(dir, &["cfg_space", "active_bars", "bar0"]).unwrap_or_default();
    let max_sqs = find_u64(dir, &["config", "max_sqs"]).unwrap_or(0);
    let max_cqs = find_u64(dir, &["config", "max_cqs"]).unwrap_or(0);
    let msix_count = find_u64(dir, &["cfg_space", "capabilities", "msi-x", "count"]).unwrap_or(0);
    let io_sqs_count = count_children(dir, &["io_sqs"]);
    let io_cqs_count = count_children(dir, &["io_cqs"]);
    let aer_outstanding = count_children(dir, &["asynchronous_event_requests"]);

    let id_str = match (vid, did) {
        (Some(v), Some(d)) => format!("VEN_{v:04X}&DEV_{d:04X}"),
        _ => "VEN_?&DEV_?".to_string(),
    };

    // Disambiguate "unit_state=running" (worker task alive) from "actually
    // serving IO". When the partition itself is halted, no guest is issuing
    // commands, so the controller is functionally idle.
    let state_str = if vm_halted && unit_state == "running" {
        "idle - VM halted".to_string()
    } else {
        unit_state
    };

    println!("Controller {name}  [{state_str}]  {id_str}");
    if !subsys.is_empty() {
        // Trim to first 8 hex chars for brevity.
        let s = subsys
            .chars()
            .take(8)
            .collect::<String>();
        println!("  Subsystem: {s}...  BAR0: {bar0}");
    } else {
        println!("  BAR0: {bar0}");
    }
    println!(
        "  Queues: max_sqs={max_sqs}, max_cqs={max_cqs}  | io_sqs={io_sqs_count}, io_cqs={io_cqs_count}  | msi-x_vectors={msix_count}"
    );
    println!("  Outstanding AERs (queued by host): {aer_outstanding}");

    // Namespaces.
    let ns_entries = find_dir(dir, &["namespaces"]);
    match ns_entries {
        None | Some([]) => {
            println!("  Namespaces: (none)");
        }
        Some(list) => {
            println!("  Namespaces:");
            for entry in list {
                print_namespace(&entry.name, &entry.node);
            }
        }
    }
}

fn print_namespace(name: &str, node: &Node) {
    let dir = match node {
        Node::Dir(d) => d,
        _ => {
            println!("    NSID {name}: <unevaluated>");
            return;
        }
    };

    let nsid = find_u64(dir, &["nsid"]).unwrap_or(0);
    let block_shift = find_u64(dir, &["block_shift"]).unwrap_or(0);
    let sector_size = find_u64(dir, &["disk", "sector_size"]).unwrap_or(1u64 << block_shift);
    let sector_count = find_u64(dir, &["disk", "sector_count"]).unwrap_or(0);
    let disk_type = find_string(dir, &["disk", "disk_type"]).unwrap_or_else(|| "?".to_string());
    let read_only = find_bool(dir, &["disk", "is_read_only"]).unwrap_or(false);

    let total_bytes = sector_count.saturating_mul(sector_size);
    println!(
        "    NSID {nsid}  [{disk_type}{ro}]  size={size}  (sectors={sector_count}, LBA={sector_size}B)",
        nsid = nsid,
        disk_type = disk_type,
        ro = if read_only { ",RO" } else { "" },
        size = human_bytes(total_bytes),
        sector_count = sector_count,
        sector_size = sector_size,
    );
}

// ----- inspect tree walk helpers ----------------------------------------

fn find_node<'a>(entries: &'a [inspect::Entry], path: &[&str]) -> Option<&'a Node> {
    let mut cur: &[inspect::Entry] = entries;
    let mut node: Option<&Node> = None;
    for segment in path {
        let e = cur.iter().find(|e| e.name == *segment)?;
        node = Some(&e.node);
        cur = match &e.node {
            Node::Dir(v) => v.as_slice(),
            _ => &[],
        };
    }
    node
}

fn find_dir<'a>(entries: &'a [inspect::Entry], path: &[&str]) -> Option<&'a [inspect::Entry]> {
    match find_node(entries, path)? {
        Node::Dir(v) => Some(v.as_slice()),
        _ => None,
    }
}

fn find_value<'a>(entries: &'a [inspect::Entry], path: &[&str]) -> Option<&'a Value> {
    match find_node(entries, path)? {
        Node::Value(v) => Some(v),
        _ => None,
    }
}

fn find_u64(entries: &[inspect::Entry], path: &[&str]) -> Option<u64> {
    match &find_value(entries, path)?.kind {
        ValueKind::Unsigned(u) => Some(*u),
        ValueKind::Signed(i) => Some(*i as u64),
        _ => None,
    }
}

fn find_bool(entries: &[inspect::Entry], path: &[&str]) -> Option<bool> {
    match &find_value(entries, path)?.kind {
        ValueKind::Bool(b) => Some(*b),
        _ => None,
    }
}

fn find_string(entries: &[inspect::Entry], path: &[&str]) -> Option<String> {
    let v = find_value(entries, path)?;
    match &v.kind {
        ValueKind::String(s) => Some(s.clone()),
        _ => Some(format!("{v}")),
    }
}

fn count_children(entries: &[inspect::Entry], path: &[&str]) -> usize {
    find_dir(entries, path).map(|v| v.len()).unwrap_or(0)
}

fn human_bytes(b: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    const TIB: u64 = GIB * 1024;
    if b >= TIB {
        format!("{:.2} TiB", b as f64 / TIB as f64)
    } else if b >= GIB {
        format!("{:.2} GiB", b as f64 / GIB as f64)
    } else if b >= MIB {
        format!("{:.2} MiB", b as f64 / MIB as f64)
    } else if b >= KIB {
        format!("{:.2} KiB", b as f64 / KIB as f64)
    } else {
        format!("{b} B")
    }
}
