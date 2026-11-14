//j895
//! Host-driven live QEMU NVMe proof of the common queue/SMART code (no guest/SMMU claim).
use drv_nvme::{
    admin::{AdminIo, Identify, Queue},
    smart, Error, Memory, RegisterIo, Startup, PAGE,
};
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
const BAR: u64 = 0xf000_0000;
const RAM: u64 = 0x0100_0000;
struct Vm {
    process: Child,
    socket: PathBuf,
}
impl Drop for Vm {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
        let _ = fs::remove_file(&self.socket);
    }
}
struct Qtest {
    reader: BufReader<UnixStream>,
    started: Instant,
}
impl Qtest {
    fn call(&mut self, command: &str) -> String {
        writeln!(self.reader.get_mut(), "{command}").unwrap();
        loop {
            let mut line = String::new();
            assert!(
                self.reader.read_line(&mut line).unwrap() > 0,
                "QEMU disconnected"
            );
            if line.starts_with("IRQ ") {
                continue;
            }
            assert!(line.starts_with("OK"), "{command}: {line}");
            return line.trim().to_owned();
        }
    }
    fn value(&mut self, c: &str) -> u64 {
        let r = self.call(c);
        u64::from_str_radix(
            r.split_whitespace()
                .nth(1)
                .unwrap()
                .trim_start_matches("0x"),
            16,
        )
        .unwrap()
    }
    fn pci(&mut self, offset: u32, value: Option<u32>) -> u32 {
        self.call(&format!(
            "outl 0xcf8 0x{:x}",
            0x8000_0000u32 | (4 << 11) | offset
        ));
        if let Some(v) = value {
            self.call(&format!("outl 0xcfc 0x{v:x}"));
            v
        } else {
            self.value("inl 0xcfc") as u32
        }
    }
}
impl RegisterIo for Qtest {
    fn read32(&mut self, o: u16) -> u32 {
        self.value(&format!("readl 0x{:x}", BAR + u64::from(o))) as u32
    }
    fn write32(&mut self, o: u16, v: u32) {
        self.call(&format!("writel 0x{:x} 0x{v:x}", BAR + u64::from(o)));
    }
    fn barrier(&mut self) {} // Each protocol transaction completes before the next is sent.
    fn now_ns(&mut self) -> u64 {
        self.started.elapsed().as_nanos() as u64 + 1
    }
    fn sleep_ns(&mut self, n: u64) -> bool {
        std::thread::sleep(Duration::from_nanos(n.min(1_000_000)));
        true
    }
}
impl AdminIo for Qtest {
    fn dma_read32(&mut self, o: u64) -> u32 {
        self.value(&format!("readl 0x{:x}", RAM + o)) as u32
    }
    fn dma_write32(&mut self, o: u64, v: u32) {
        self.call(&format!("writel 0x{:x} 0x{v:x}", RAM + o));
    }
    fn dma_acquire(&mut self) {} // No local DMA alias: protocol reads observe QEMU's RAM.
}
fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("output directory"));
    fs::create_dir_all(&dir).unwrap();
    let disk = dir.join("disk.raw");
    let mut f = File::create(&disk).unwrap();
    f.set_len(8 * 1024 * 1024).unwrap();
    f.write_all(b"KUMO-SMART-READ\n").unwrap();
    drop(f);
    let socket = PathBuf::from(format!("/tmp/kumo-smart-{}.sock", std::process::id()));
    let log = File::create(dir.join("qemu.log")).unwrap();
    let process = Command::new("qemu-system-x86_64")
        .args([
            "-machine",
            "q35,accel=tcg",
            "-m",
            "64M",
            "-nodefaults",
            "-display",
            "none",
            "-S",
            "-qtest",
        ])
        .arg(format!("unix:{},server=on,wait=off", socket.display()))
        .args(["-qtest-log", "/dev/null", "-drive"])
        .arg(format!("file={},if=none,id=d,format=raw", disk.display()))
        .args([
            "-device",
            "nvme,drive=d,serial=KUMOSMART,addr=04.0,smart_critical_warning=2",
        ])
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap();
    let _vm = Vm {
        process,
        socket: socket.clone(),
    };
    let start = Instant::now();
    let stream = loop {
        match UnixStream::connect(&socket) {
            Ok(s) => break s,
            Err(_) if start.elapsed() < Duration::from_secs(5) => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(e) => panic!("qtest unavailable: {e}"),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut io = Qtest {
        reader: BufReader::new(stream),
        started: Instant::now(),
    };
    let id = io.pci(0, None);
    assert_ne!(id, u32::MAX);
    io.pci(0x10, Some(BAR as u32));
    io.pci(0x14, Some(0));
    io.pci(4, Some(6));
    let cap = u64::from(io.read32(0)) | (u64::from(io.read32(4)) << 32);
    let vs = io.read32(8);
    let s = Startup {
        session: 1,
        bar: BAR,
        cap,
        vs,
        id,
        class_revision: 0x01080200,
        root_command: 6,
        endpoint_command: 6,
        msix_control: 0,
        irq: 292,
        pin: 1,
        legacy_mask: 0,
        mode: 0,
    };
    s.validate().unwrap();
    let memory = Memory {
        session: 1,
        physical: RAM,
    };
    for off in (0..drv_nvme::DMA_BYTES).step_by(4) {
        io.dma_write32(off, 0);
    }
    let p = drv_nvme::exercise_with(&mut io, s, memory, |io| {
        let mut admin = Queue::new();
        let mut data = [0; PAGE as usize];
        admin.identify(io, s, memory, Identify::Controller, &mut data)?;
        let limits = smart::Limits::decode(&data)?;
        admin.identify(io, s, memory, Identify::ActiveNamespaces, &mut data)?;
        let nsid = drv_nvme::admin::first_namespace(&data)?;
        admin.identify(io, s, memory, Identify::Namespace(nsid), &mut data)?;
        let ns = drv_nvme::admin::Namespace::decode(nsid, &data)?;
        let mut pair = drv_nvme::io::IoPair::open(&mut admin, io, s, memory, ns, false)?;
        let mut first = 0;
        let mut last = 0;
        for i in 0..40 {
            let (h, c) = smart::read(io, &mut admin, s, memory, limits)?;
            assert_eq!(h.state, smart::State::Ok);
            assert!(h.fresh(io.now_ns()));
            assert!(h.log.thermal_warning());
            assert!(smart::millicelsius(h.log.composite).is_some());
            if i == 0 {
                first = h.sampled_ns;
            }
            last = h.sampled_ns;
            let mut text = String::new();
            h.write_status(&mut text, io.now_ns()).unwrap();
            if i == 0 || i == 39 {
                println!("QTEST SMART cid={} {text}", c.cid);
            }
            let mut block = [0; 512];
            pair.read(io, s, memory, 0, 1, &mut block)?;
            assert_eq!(&block[..16], b"KUMO-SMART-READ\n");
            if i == 3 {
                let mut invalid = [0; 16];
                invalid[0] = 0xff;
                let mut output = [0xaa; 512];
                let rejected = admin.submit_admin_status(io, s, memory, invalid, &mut output)?;
                assert_ne!(rejected.status >> 1, 0);
                assert!(!admin.poisoned());
                assert!(output.iter().all(|v| *v == 0));
                println!("QTEST SMART rejected opcode status=0x{:x}; CQ released, next SMART and block read must succeed",rejected.status>>1);
            }
        }
        assert!(last > first);
        pair.close(&mut admin, io, s, memory)?;
        Ok::<(), Error>(())
    });
    assert!(p.complete(), "{p}");
    println!("QTEST SMART green: 40 fresh health reads interleaved with 40 exact block reads, phase wrap, completed rejection recovery, clean disable");
}
