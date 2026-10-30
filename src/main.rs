mod log;
mod offsets;
mod overview;
mod web;

use anyhow::{Context, Result, anyhow};
use log::Show;
use memprocfs::{FLAG_NOCACHE, Vmm, VmmProcess};
use offsets::Offsets;
use serde_json::json;
use std::{
    cell::Cell,
    io::BufRead,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

/// Size of a CEntityIdentity in the entity list chunks. Valve has changed this
/// before (0x78 -> 0x70), so both are tried and the one that validates wins.
const IDENTITY_STRIDES: [u64; 2] = [0x70, 0x78];
const MAX_PLAYERS: u32 = 64;

#[derive(Clone, Copy)]
#[repr(C)]
struct Vec3 {
    x: f32,
    y: f32,
    z: f32,
}

impl Show for Vec3 {
    fn show(&self) -> String {
        format!("({:.1}, {:.1}, {:.1})", self.x, self.y, self.z)
    }
}

struct Player {
    index: u32,
    name: String,
    team: u8,
    health: i32,
    armor: i32,
    alive: bool,
    pos: Vec3,
    yaw: f32,
    is_local: bool,
    /// Competitive teammate color: -1 none, 0..4 = blue, green, yellow, orange, purple.
    color: i32,
}

struct Game<'a> {
    proc: VmmProcess<'a>,
    client: u64,
    off: Offsets,
    stride: u64,
    /// Print every read of the entity list walk.
    trace: bool,
    /// Where the map name pointer sits inside GlobalVars, once found.
    map_name_offset: Cell<Option<u64>>,
}

/// "maps/de_dust2.vpk" or "de_dust2" -> Some("de_dust2"); anything else -> None.
fn parse_map_name(s: &str) -> Option<String> {
    let base = s.rsplit(['/', '\\']).next()?;
    let base = base.trim_end_matches(".vpk").trim_end_matches(".bsp");
    let prefixes = ["de_", "cs_", "ar_", "dz_", "gd_", "aim_", "awp_", "workshop"];
    let valid = base.len() >= 4
        && base.len() <= 48
        && base.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && prefixes.iter().any(|p| base.starts_with(p));
    valid.then(|| base.to_string())
}

impl<'a> Game<'a> {
    fn read<T>(&self, va: u64) -> Result<T> {
        self.proc.mem_read_as::<T>(va, FLAG_NOCACHE)
    }

    fn read_ptr(&self, va: u64) -> Result<u64> {
        let p: u64 = self.read(va)?;
        if p == 0 { Err(anyhow!("null pointer at {va:#x}")) } else { Ok(p) }
    }

    fn read_string(&self, va: u64, max: usize) -> Result<String> {
        let bytes = self.proc.mem_read_ex(va, max, FLAG_NOCACHE)?;
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        Ok(String::from_utf8_lossy(&bytes[..end]).into_owned())
    }

    fn entity_list(&self) -> Result<u64> {
        step!(
            self.trace,
            self.read_ptr(self.client + self.off.dw_entity_list),
            "read entity list pointer at client.dll+{:#x}",
            self.off.dw_entity_list
        )
    }

    /// Resolve an entity index (or the low 15 bits of a handle) to its address.
    /// Stale/empty slots are rejected by checking the entity points back at
    /// its own identity.
    fn entity_by_index(&self, list: u64, index: u32, stride: u64) -> Result<u64> {
        Ok(self.identity_by_index(list, index, stride)?.0)
    }

    /// Returns (entity, identity) for an entity index.
    fn identity_by_index(&self, list: u64, index: u32, stride: u64) -> Result<(u64, u64)> {
        let t = self.trace;
        let index = (index & 0x7FFF) as u64;
        let (chunk_no, slot) = (index >> 9, index & 0x1FF);
        let chunk = step!(
            t,
            self.read_ptr(list + 8 * chunk_no + 0x10),
            "    entity {index}: read chunk[{chunk_no}] pointer"
        )?;
        let identity = chunk + stride * slot;
        let ent = step!(
            t,
            self.read_ptr(identity),
            "    entity {index}: read identity[{slot}] at {identity:#x} -> entity"
        )?;
        step!(
            t,
            self.read::<u64>(ent + 0x10).and_then(|p| {
                if p == identity { Ok(p) } else { Err(anyhow!("points to {p:#x}, expected {identity:#x}")) }
            }),
            "    entity {index}: back-pointer entity+0x10 == identity"
        )?;
        Ok((ent, identity))
    }

    /// CEntityIdentity::m_designerName, e.g. "cs_player_controller".
    fn designer_name(&self, identity: u64) -> Result<String> {
        let p = self.read_ptr(identity + 0x20)?;
        self.read_string(p, 64)
    }

    /// Current map, e.g. "de_dust2". GlobalVars holds a pointer to the map
    /// name, but its offset isn't in the dump, so the struct is scanned for a
    /// pointer to a string that looks like a map name and the hit is cached.
    fn map_name(&self) -> Result<String> {
        let t = self.trace;
        let gv = step!(
            t,
            self.read_ptr(self.client + self.off.dw_global_vars),
            "read GlobalVars pointer at client.dll+{:#x}",
            self.off.dw_global_vars
        )?;
        let at = |off: u64| -> Option<String> {
            let p = self.read_ptr(gv + off).ok()?;
            parse_map_name(&self.read_string(p, 64).ok()?)
        };

        if let Some(off) = self.map_name_offset.get()
            && let Some(name) = at(off)
        {
            return step!(t, Ok(name), "read map name at GlobalVars+{off:#x}");
        }
        let bytes = self.proc.mem_read_ex(gv, 0x400, FLAG_NOCACHE)?;
        let found = (0..0x400u64).step_by(8).find_map(|off| {
            let p = u64::from_le_bytes(bytes[off as usize..off as usize + 8].try_into().unwrap());
            if !(0x10000..0x7FFF_FFFF_FFFF).contains(&p) {
                return None;
            }
            at(off).map(|name| (off, name))
        });
        match found {
            Some((off, name)) => {
                self.map_name_offset.set(Some(off));
                step!(t, Ok(name), "scan GlobalVars for map name: found at +{off:#x}")
            }
            None => step!(t, Err(anyhow!("none found (main menu?)")), "scan GlobalVars for map name"),
        }
    }

    /// Each entity points back at its CEntityIdentity (entity + 0x10), which
    /// lets us check which identity stride the current build uses.
    fn detect_stride(&mut self) -> Result<()> {
        println!("== Detecting entity identity size ==");
        let list = self.entity_list()?;
        let trace = std::mem::replace(&mut self.trace, false);
        let found = IDENTITY_STRIDES.into_iter().find(|&stride| {
            let hit = (1..=MAX_PLAYERS).find(|&i| self.identity_by_index(list, i, stride).is_ok());
            let hit = hit.ok_or_else(|| anyhow!("no slot in 1..={MAX_PLAYERS} validates"));
            step!(true, hit.map(|i| format!("slot {i} validates")), "  try stride {stride:#x}").is_ok()
        });
        self.trace = trace;
        self.stride = found.ok_or_else(|| anyhow!("could not validate entity list (not in a match yet?)"))?;
        Ok(())
    }

    fn read_player(&self, list: u64, index: u32, local_controller: u64) -> Result<Player> {
        let (o, t) = (&self.off, self.trace);
        let (controller, identity) = self.identity_by_index(list, index, self.stride)?;
        step!(
            t,
            self.designer_name(identity).and_then(|c| {
                if c == "cs_player_controller" { Ok(c) } else { Err(anyhow!("is {c:?}, not a player")) }
            }),
            "    controller type"
        )?;

        let pawn_handle = step!(
            t,
            self.read::<u32>(controller + o.m_h_player_pawn).and_then(|h| {
                if h == u32::MAX { Err(anyhow!("0xffffffff (no pawn)")) } else { Ok(h) }
            }),
            "    read m_hPlayerPawn handle"
        )?;
        if t {
            println!("    pawn handle -> entity index {}", pawn_handle & 0x7FFF);
        }
        let pawn = self.entity_by_index(list, pawn_handle, self.stride)?;

        let name = step!(
            t,
            self.read_string(controller + o.m_isz_player_name, 128).and_then(|n| {
                if n.is_empty() { Err(anyhow!("empty")) } else { Ok(n) }
            }),
            "    read m_iszPlayerName"
        )?;
        let alive = step!(t, self.read::<u8>(controller + o.m_b_pawn_is_alive), "    read m_bPawnIsAlive")?;
        let armor = step!(t, self.read::<i32>(controller + o.m_i_pawn_armor), "    read m_iPawnArmor")?;
        let color = step!(t, self.read::<i32>(controller + o.m_i_comp_teammate_color), "    read m_iCompTeammateColor")?;
        let health = step!(t, self.read::<i32>(pawn + o.m_i_health), "    read pawn m_iHealth")?;
        let team = step!(t, self.read::<u8>(pawn + o.m_i_team_num), "    read pawn m_iTeamNum")?;
        let pos = match step!(t, self.read_ptr(pawn + o.m_p_game_scene_node), "    read m_pGameSceneNode") {
            Ok(node) => step!(t, self.read::<Vec3>(node + o.m_vec_abs_origin), "    read node m_vecAbsOrigin")?,
            Err(_) => step!(t, self.read::<Vec3>(pawn + o.m_v_old_origin), "    read pawn m_vOldOrigin (fallback)")?,
        };
        let angles = step!(t, self.read::<Vec3>(pawn + o.m_ang_eye_angles), "    read m_angEyeAngles")?;

        Ok(Player {
            index,
            name,
            team,
            health,
            armor,
            alive: alive != 0 && health > 0,
            pos,
            yaw: angles.y,
            is_local: controller == local_controller,
            color,
        })
    }

    fn players(&self) -> Result<Vec<Player>> {
        let t = self.trace;
        if t {
            println!("== Walking entity list (slots 1..={MAX_PLAYERS}) ==");
        }
        let list = self.entity_list()?;
        let local = step!(
            t,
            self.read_ptr(self.client + self.off.dw_local_player_controller),
            "read local player controller"
        )
        .unwrap_or(0);

        let mut players = Vec::new();
        for i in 1..=MAX_PLAYERS {
            if t {
                println!("  -- slot {i} --");
            }
            match self.read_player(list, i, local) {
                Ok(p) => {
                    if t {
                        println!("{}   slot {i}: player {:?}", log::OK, p.name);
                    }
                    players.push(p);
                }
                Err(_) if t => println!("{}   slot {i}: skipped", log::FAIL),
                Err(_) => {}
            }
        }
        if t {
            println!("== Found {} players ==", players.len());
        }
        Ok(players)
    }
}

fn team_name(t: u8) -> &'static str {
    match t {
        2 => "T",
        3 => "CT",
        1 => "SPEC",
        _ => "?",
    }
}

fn find_vmm_dll() -> PathBuf {
    let next_to_exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("vmm.dll")));
    match next_to_exe {
        Some(p) if p.exists() => p,
        _ => PathBuf::from("vmm.dll"),
    }
}

fn players_json(map: &str, players: &[Player]) -> String {
    let list: Vec<_> = players
        .iter()
        .map(|p| {
            json!({
                "index": p.index, "name": p.name, "team": p.team,
                "health": p.health, "armor": p.armor, "alive": p.alive,
                "x": p.pos.x, "y": p.pos.y, "z": p.pos.z, "yaw": p.yaw,
                "local": p.is_local, "color": p.color,
            })
        })
        .collect();
    json!({ "map": map, "players": list }).to_string()
}

fn print_table(players: &[Player]) {
    println!(
        "{:>3} {:<24} {:<4} {:>4} {:>4} {:>10} {:>10} {:>10} {:>7}",
        "idx", "name", "team", "hp", "ap", "x", "y", "z", "yaw"
    );
    for p in players {
        println!(
            "{:>3} {:<24} {:<4} {:>4} {:>4} {:>10.1} {:>10.1} {:>10.1} {:>7.1}{}{}",
            p.index,
            p.name.chars().take(24).collect::<String>(),
            team_name(p.team),
            p.health,
            p.armor,
            p.pos.x,
            p.pos.y,
            p.pos.z,
            p.yaw,
            if p.alive { "" } else { "  (dead)" },
            if p.is_local { "  <- you" } else { "" },
        );
    }
    println!("\n{} players", players.len());
}

fn main() -> Result<()> {
    // Usage: cs2_radar [device] [--trace] [--lan] [--port N]
    //   device    MemProcFS device, default "fpga" (e.g. "fpga://algo=0", "file.raw")
    //   --trace   log every step on every refresh instead of only the first scan
    //   --lan     let other devices on your network open the radar page
    //   --port N  web server port, default 8080
    let args: Vec<String> = std::env::args().skip(1).collect();
    let trace_always = args.iter().any(|a| a == "--trace");
    let lan = args.iter().any(|a| a == "--lan");
    let port = args
        .iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .map(|p| p.parse::<u16>().context("--port must be a number"))
        .transpose()?
        .unwrap_or(8080);
    let device = args
        .iter()
        .enumerate()
        .find(|(i, a)| !a.starts_with("--") && (*i == 0 || args[i - 1] != "--port"))
        .map(|(_, a)| a.clone())
        .unwrap_or_else(|| "fpga".into());

    let off = Offsets::load()?;

    println!("== Connecting ==");
    let dll = step!(
        true,
        find_vmm_dll().to_str().map(String::from).context("vmm.dll path is not valid UTF-8"),
        "locate vmm.dll"
    )?;
    let vmm = match Vmm::new(&dll, &vec!["", "-device", &device, "-waitinitialize"]) {
        Ok(v) => {
            println!("{} initialize MemProcFS with device {device:?}", log::OK);
            v
        }
        Err(e) => {
            println!("{} initialize MemProcFS with device {device:?}: {e:#}", log::FAIL);
            return Err(e.context("is the DMA device connected?"));
        }
    };

    let proc = loop {
        match vmm.process_from_name("cs2.exe") {
            Ok(p) => {
                println!("{} find process cs2.exe -> pid {}", log::OK, p.pid);
                break p;
            }
            Err(e) => {
                println!("{} find process cs2.exe: {e:#} (retrying in 2s)", log::FAIL);
                thread::sleep(Duration::from_secs(2));
            }
        }
    };
    let client = step!(
        true,
        proc.get_module_base("client.dll").context("is the game fully loaded?"),
        "find client.dll base"
    )?;

    let mut game = Game {
        proc,
        client,
        off,
        stride: IDENTITY_STRIDES[0],
        trace: true,
        map_name_offset: Cell::new(None),
    };
    while let Err(e) = game.detect_stride() {
        println!("{} detect stride: {e:#} (retrying in 2s)", log::FAIL);
        thread::sleep(Duration::from_secs(2));
    }

    // First scan always shows every step; afterwards only with --trace.
    println!("== Reading map name ==");
    let mut map = game.map_name().unwrap_or_default();
    match game.players() {
        Ok(players) => print_table(&players),
        Err(e) => println!("{} first scan: {e:#}", log::FAIL),
    }
    if !trace_always {
        println!("\nPress Enter to start the live radar (run with --trace to keep logging every step)...");
        let _ = std::io::stdin().lock().lines().next();
        game.trace = false;
    }

    println!("== Starting web server ==");
    let snapshot: web::Snapshot = Arc::new(Mutex::new(players_json(&map, &[])));
    let addr = format!("{}:{port}", if lan { "0.0.0.0" } else { "127.0.0.1" });
    step!(
        true,
        web::start(&addr, snapshot.clone()).map(|maps| format!("{maps} maps with radar images")),
        "listen on {addr}"
    )?;
    let url = format!("http://localhost:{port}");
    println!("Open {url} in a browser{}", if lan { " (or this PC's IP from another device)" } else { "" });
    if !trace_always {
        thread::sleep(Duration::from_secs(2));
    }

    // Read the game every 100 ms for a smooth radar; redraw the console every
    // 500 ms and re-check the map once a second.
    let mut tick = 0u64;
    loop {
        tick += 1;
        if trace_always || tick.is_multiple_of(10) {
            map = game.map_name().unwrap_or_default();
        }
        match game.players() {
            Ok(players) => {
                *snapshot.lock().unwrap() = players_json(&map, &players);
                if trace_always || tick.is_multiple_of(5) {
                    if !trace_always {
                        print!("\x1b[2J\x1b[H");
                    }
                    println!("Radar: {url}    Map: {}\n", if map.is_empty() { "?" } else { &map });
                    print_table(&players);
                }
            }
            Err(e) => println!("{} scan: {e:#}", log::FAIL),
        }
        thread::sleep(Duration::from_millis(if trace_always { 1000 } else { 100 }));
    }
}
