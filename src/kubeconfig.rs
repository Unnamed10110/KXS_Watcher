//! Context discovery across every kubeconfig we can find, and client construction.
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use kube::config::{KubeConfigOptions, Kubeconfig};

#[derive(Clone, Debug, PartialEq)]
pub struct Ctx {
    pub file: PathBuf,
    pub name: String,
    pub server: String,
    pub namespace: Option<String>,
}

impl Ctx {
    /// Stable id (same context name can exist in several files).
    pub fn id(&self) -> String {
        format!("{}|{}", self.file.display(), self.name)
    }
}

pub fn home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from)
}

pub fn default_dir() -> PathBuf {
    home().unwrap_or_default().join(".kube")
}

/// `$KUBECONFIG` entries, `~/.kube/config`, then user paths (folders scanned one level deep).
pub fn candidate_files(kubeconfig_env: Option<&std::ffi::OsStr>, extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = kubeconfig_env.map(|v| std::env::split_paths(v).collect()).unwrap_or_default();
    out.push(default_dir().join("config"));
    for p in extra {
        if p.is_dir() {
            let mut files: Vec<PathBuf> = std::fs::read_dir(p)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|f| f.is_file())
                .collect();
            files.sort();
            out.extend(files);
        } else {
            out.push(p.clone());
        }
    }
    let mut seen = HashSet::new();
    out.retain(|p| !p.as_os_str().is_empty() && seen.insert(std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())));
    out
}

pub fn discover(extra: &[PathBuf]) -> Vec<Ctx> {
    candidate_files(std::env::var_os("KUBECONFIG").as_deref(), extra)
        .iter()
        .flat_map(|f| contexts_in(f))
        .collect()
}

/// Contexts of one file. Unreadable / non-kubeconfig files yield nothing.
pub fn contexts_in(file: &Path) -> Vec<Ctx> {
    // Skip big files (logs, binaries) that happen to live in the folder.
    if std::fs::metadata(file).map(|m| m.len() > 4 << 20).unwrap_or(true) {
        return vec![];
    }
    let Ok(kc) = Kubeconfig::read_from(file) else { return vec![] };
    kc.contexts
        .iter()
        .map(|nc| {
            let c = nc.context.as_ref();
            let server = c
                .and_then(|c| kc.clusters.iter().find(|cl| cl.name == c.cluster))
                .and_then(|cl| cl.cluster.as_ref()?.server.clone())
                .unwrap_or_default();
            Ctx { file: file.to_path_buf(), name: nc.name.clone(), server, namespace: c.and_then(|c| c.namespace.clone()) }
        })
        .collect()
}

/// Client for exactly this context (never relies on `current-context`).
pub async fn client(ctx: &Ctx) -> anyhow::Result<kube::Client> {
    let kc = Kubeconfig::read_from(&ctx.file)?;
    let opts = KubeConfigOptions { context: Some(ctx.name.clone()), ..Default::default() };
    let mut cfg = kube::Config::from_custom_kubeconfig(kc, &opts).await?;
    // Log follows may be silent for a long time; watches have their own stall timeout.
    cfg.read_timeout = None;
    Ok(kube::Client::try_from(cfg)?)
}

/// `KUBECONFIG` value for a terminal pinned to `ctx` without copying credentials:
/// a tiny file that only sets current-context goes first (kubectl takes the first one set).
pub fn terminal_kubeconfig(ctx: &Ctx) -> std::io::Result<String> {
    let dir = std::env::temp_dir().join("kxs-watcher");
    std::fs::create_dir_all(&dir)?;
    let safe: String = ctx.name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    let pin = dir.join(format!("{safe}.yaml"));
    std::fs::write(&pin, format!("apiVersion: v1\nkind: Config\ncurrent-context: \"{}\"\n", ctx.name.replace('"', "\\\"")))?;
    Ok(std::env::join_paths([pin.as_path(), ctx.file.as_path()]).map_err(std::io::Error::other)?.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KC: &str = r#"
apiVersion: v1
kind: Config
current-context: missing
clusters:
- name: c1
  cluster: { server: "https://10.0.0.1:6443" }
contexts:
- name: qa
  context: { cluster: c1, user: u1, namespace: team }
- name: orphan
  context: { cluster: nope, user: u1 }
users:
- name: u1
  user: { token: abc }
"#;

    #[test]
    fn discovers_contexts_and_skips_junk() {
        let dir = std::env::temp_dir().join(format!("kxs-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("cache")).unwrap();
        std::fs::write(dir.join("config-qa"), KC).unwrap();
        std::fs::write(dir.join("junk.bin"), [0u8, 159, 146, 150]).unwrap();

        let files = candidate_files(None, &[dir.clone()]);
        assert!(files.iter().any(|f| f.ends_with("config-qa")));
        assert!(!files.iter().any(|f| f.ends_with("cache")));

        let ctxs: Vec<Ctx> = files.iter().filter(|f| f.starts_with(&dir)).flat_map(|f| contexts_in(f)).collect();
        assert_eq!(ctxs.len(), 2);
        assert_eq!(ctxs[0].name, "qa");
        assert_eq!(ctxs[0].server, "https://10.0.0.1:6443");
        assert_eq!(ctxs[0].namespace.as_deref(), Some("team"));
        assert_eq!(ctxs[1].server, "");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn splits_kubeconfig_env_and_dedupes() {
        let joined = std::env::join_paths(["a.yaml", "b.yaml", "a.yaml"]).unwrap();
        let files = candidate_files(Some(&joined), &[]);
        assert_eq!(files[0], PathBuf::from("a.yaml"));
        assert_eq!(files[1], PathBuf::from("b.yaml"));
        assert_eq!(files.iter().filter(|f| f.ends_with("a.yaml")).count(), 1);
    }
}
