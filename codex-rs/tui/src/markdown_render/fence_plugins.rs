use ansi_to_tui::IntoText;
use codex_utils_home_dir::find_codex_home;
use ratatui::text::Line;
use std::collections::HashMap;
use std::io::Read;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;

const TIMEOUT: Duration = Duration::from_secs(3);
const CACHE_LIMIT: usize = 256;

struct Renderer {
    plugin_root: PathBuf,
    command: String,
}

fn renderers() -> &'static HashMap<String, Renderer> {
    static RENDERERS: OnceLock<HashMap<String, Renderer>> = OnceLock::new();
    RENDERERS.get_or_init(|| {
        let mut found = HashMap::new();
        let Ok(home) = find_codex_home() else {
            return found;
        };
        let cache = home.as_path().join("plugins").join("cache");
        for manifest in glob_manifests(&cache) {
            let Some(plugin_root) = manifest.parent().and_then(|dir| dir.parent()) else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(&manifest) else {
                continue;
            };
            let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            let Some(entries) = json.get("fenceRenderers").and_then(|v| v.as_object()) else {
                continue;
            };
            for (lang, entry) in entries {
                if let Some(command) = entry.get("command").and_then(|c| c.as_str()) {
                    found.entry(lang.clone()).or_insert(Renderer {
                        plugin_root: plugin_root.to_path_buf(),
                        command: command.to_string(),
                    });
                }
            }
        }
        found
    })
}

fn glob_manifests(cache: &std::path::Path) -> Vec<PathBuf> {
    let children = |dir: &std::path::Path| -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default()
    };
    children(cache)
        .iter()
        .flat_map(|marketplace| children(marketplace))
        .flat_map(|plugin| children(&plugin))
        .map(|version| version.join(".codex-plugin").join("plugin.json"))
        .filter(|manifest| manifest.is_file())
        .collect()
}

pub(super) fn render(lang: &str, code: &str, width: Option<usize>) -> Option<Vec<Line<'static>>> {
    let renderer = renderers().get(lang)?;
    let width = width.unwrap_or(100);
    static CACHE: OnceLock<Mutex<HashMap<(String, String, usize), Option<Vec<Line<'static>>>>>> =
        OnceLock::new();
    let key = (lang.to_string(), code.to_string(), width);
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().ok()?.get(&key) {
        return hit.clone();
    }
    let drawn = run(renderer, code, width);
    let mut cache = cache.lock().ok()?;
    if cache.len() >= CACHE_LIMIT {
        cache.clear();
    }
    cache.insert(key, drawn.clone());
    drawn
}

fn run(renderer: &Renderer, code: &str, width: usize) -> Option<Vec<Line<'static>>> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&renderer.command)
        .env("PLUGIN_ROOT", &renderer.plugin_root)
        .env("COLUMNS", width.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .ok()?;
    let group = child.id() as i32;
    let kill = |child: &mut std::process::Child| {
        unsafe { libc::kill(-group, libc::SIGKILL) };
        let _ = child.wait();
    };
    let mut stdin = child.stdin.take()?;
    let input = code.to_string();
    std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let mut stdout = child.stdout.take()?;
    let (sent, received) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        let _ = sent.send(bytes);
    });
    let deadline = Instant::now() + TIMEOUT;
    let Ok(bytes) = received.recv_timeout(TIMEOUT) else {
        kill(&mut child);
        return None;
    };
    let status = loop {
        if let Some(status) = child.try_wait().ok()? {
            break status;
        }
        if Instant::now() > deadline {
            kill(&mut child);
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    if !status.success() || bytes.is_empty() {
        return None;
    }
    let lines = bytes.into_text().ok()?.lines;
    if lines.iter().any(|line| line.width() > width) {
        return None;
    }
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn renderer(command: &str) -> Renderer {
        Renderer {
            plugin_root: PathBuf::from("/"),
            command: command.to_string(),
        }
    }

    #[test]
    fn output_past_the_pipe_buffer_is_read() {
        let lines = run(
            &renderer("cat >/dev/null; head -c 100000 /dev/zero | tr '\\0' '\\n'"),
            &"x".repeat(200_000),
            80,
        );
        assert_eq!(lines.map(|l| l.len()), Some(100_000));
    }

    #[test]
    fn a_hung_renderer_times_out() {
        let started = Instant::now();
        assert!(run(&renderer("sleep 30 & wait"), "x", 80).is_none());
        assert!(started.elapsed() < TIMEOUT + Duration::from_secs(1));
    }

    #[test]
    fn output_wider_than_the_block_falls_back() {
        assert!(run(&renderer("printf '%090d\\n' 0"), "x", 80).is_none());
    }
}
