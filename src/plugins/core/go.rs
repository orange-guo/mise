use std::collections::HashMap;
use std::path::{Path, PathBuf};

use eyre::Result;
use itertools::Itertools;
use versions::Versioning;

use crate::cli::args::ForgeArg;
use crate::cli::version::{ARCH, OS};
use crate::cmd::CmdLineRunner;
use crate::config::Config;
use crate::forge::Forge;
use crate::http::HTTP;
use crate::install_context::InstallContext;
use crate::plugins::core::CorePlugin;
use crate::toolset::{ToolVersion, Toolset};
use crate::ui::progress_report::SingleReport;
use crate::{cmd, env, file, hash};

#[derive(Debug)]
pub struct GoPlugin {
    core: CorePlugin,
}

impl GoPlugin {
    pub fn new() -> Self {
        Self {
            core: CorePlugin::new("go"),
        }
    }

    fn fetch_remote_versions(&self) -> Result<Vec<String>> {
        match self.core.fetch_remote_versions_from_mise() {
            Ok(Some(versions)) => return Ok(versions),
            Ok(None) => {}
            Err(e) => warn!("failed to fetch remote versions: {}", e),
        }
        CorePlugin::run_fetch_task_with_timeout(move || {
            let repo = &*env::MISE_GO_REPO;
            let output = cmd!("git", "ls-remote", "--tags", repo, "go*").read()?;
            let lines = output.split('\n');
            let versions = lines.map(|s| s.split("/go").last().unwrap_or_default().to_string())
                .filter(|s| !s.is_empty())
                .filter(|s| !regex!(r"^1($|\.0|\.0\.[0-9]|\.1|\.1rc[0-9]|\.1\.[0-9]|.2|\.2rc[0-9]|\.2\.1|.8.5rc5)$").is_match(s))
                .unique()
                .sorted_by_cached_key(|s| (Versioning::new(s), s.to_string()))
                .collect();
            Ok(versions)
        })
    }

    fn goroot(&self, tv: &ToolVersion) -> PathBuf {
        tv.install_path().join("go")
    }
    fn go_bin(&self, tv: &ToolVersion) -> PathBuf {
        self.goroot(tv).join("bin/go")
    }
    fn gopath(&self, tv: &ToolVersion) -> PathBuf {
        tv.install_path().join("packages")
    }

    fn install_default_packages(&self, tv: &ToolVersion, pr: &dyn SingleReport) -> Result<()> {
        let body = file::read_to_string(&*env::MISE_GO_DEFAULT_PACKAGES_FILE).unwrap_or_default();
        for package in body.lines() {
            let package = package.split('#').next().unwrap_or_default().trim();
            if package.is_empty() {
                continue;
            }
            pr.set_message(format!("installing default package: {}", package));
            let package = if package.contains('@') {
                package.to_string()
            } else {
                format!("{}@latest", package)
            };
            let mut env = HashMap::new();
            if *env::MISE_GO_SET_GOROOT != Some(false) {
                env.insert("GOROOT", self.goroot(tv));
            }
            if *env::MISE_GO_SET_GOPATH != Some(false) {
                env.insert("GOPATH", self.gopath(tv));
            }
            CmdLineRunner::new(self.go_bin(tv))
                .with_pr(pr)
                .arg("install")
                .arg(package)
                .envs(env)
                .execute()?;
        }
        Ok(())
    }

    fn test_go(&self, tv: &ToolVersion, pr: &dyn SingleReport) -> Result<()> {
        pr.set_message("go version".into());
        CmdLineRunner::new(self.go_bin(tv))
            .with_pr(pr)
            .arg("version")
            .execute()
    }

    fn download(&self, tv: &ToolVersion, pr: &dyn SingleReport) -> Result<PathBuf> {
        let filename = format!("go{}.{}-{}.tar.gz", tv.version, platform(), arch());
        let tarball_url = format!("{}/{}", &*env::MISE_GO_DOWNLOAD_MIRROR, &filename);
        let tarball_path = tv.download_path().join(filename);

        pr.set_message(format!("downloading {}", &tarball_url));
        HTTP.download_file(&tarball_url, &tarball_path)?;

        self.verify_tarball_checksum(&tarball_url, &tarball_path)?;

        Ok(tarball_path)
    }

    fn verify_tarball_checksum(&self, tarball_url: &str, tarball_path: &Path) -> Result<()> {
        if !*env::MISE_GO_SKIP_CHECKSUM {
            let checksum_url = format!("{}.sha256", tarball_url);
            let checksum = HTTP.get_text(checksum_url)?;
            hash::ensure_checksum_sha256(tarball_path, &checksum)?;
        }
        Ok(())
    }

    fn install(&self, tv: &ToolVersion, pr: &dyn SingleReport, tarball_path: &Path) -> Result<()> {
        let tarball = tarball_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        pr.set_message(format!("installing {}", tarball));
        file::untar(tarball_path, &tv.install_path())?;
        Ok(())
    }

    fn verify(&self, tv: &ToolVersion, pr: &dyn SingleReport) -> Result<()> {
        self.test_go(tv, pr)?;
        self.install_default_packages(tv, pr)
    }
}

impl Forge for GoPlugin {
    fn fa(&self) -> &ForgeArg {
        &self.core.fa
    }
    fn list_remote_versions(&self) -> Result<Vec<String>> {
        self.core
            .remote_version_cache
            .get_or_try_init(|| self.fetch_remote_versions())
            .cloned()
    }
    fn legacy_filenames(&self) -> Result<Vec<String>> {
        Ok(vec![".go-version".into()])
    }

    fn install_version_impl(&self, ctx: &InstallContext) -> Result<()> {
        let tarball_path = self.download(&ctx.tv, ctx.pr.as_ref())?;
        self.install(&ctx.tv, ctx.pr.as_ref(), &tarball_path)?;
        self.verify(&ctx.tv, ctx.pr.as_ref())?;

        Ok(())
    }

    fn uninstall_version_impl(&self, _pr: &dyn SingleReport, tv: &ToolVersion) -> Result<()> {
        let gopath = self.gopath(tv);
        if gopath.exists() {
            cmd!("chmod", "-R", "u+wx", gopath).run()?;
        }
        Ok(())
    }

    fn list_bin_paths(&self, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        // goroot/bin must always be included, irrespective of MISE_GO_SET_GOROOT
        let mut paths = vec![self.goroot(tv).join("bin")];
        if *env::MISE_GO_SET_GOPATH != Some(false) {
            paths.push(self.gopath(tv).join("bin"));
        }
        Ok(paths)
    }

    fn exec_env(
        &self,
        _config: &Config,
        _ts: &Toolset,
        tv: &ToolVersion,
    ) -> Result<HashMap<String, String>> {
        let mut map = HashMap::new();
        match (*env::MISE_GO_SET_GOROOT, env::PRISTINE_ENV.get("GOROOT")) {
            (Some(false), _) | (None, Some(_)) => {}
            (Some(true), _) | (None, None) => {
                let goroot = self.goroot(tv).to_string_lossy().to_string();
                map.insert("GOROOT".to_string(), goroot);
            }
        };
        match (*env::MISE_GO_SET_GOPATH, env::PRISTINE_ENV.get("GOPATH")) {
            (Some(false), _) | (None, Some(_)) => {}
            (Some(true), _) | (None, None) => {
                let gopath = self.gopath(tv).to_string_lossy().to_string();
                map.insert("GOPATH".to_string(), gopath);
            }
        };
        Ok(map)
    }
}

fn platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else {
        &OS
    }
}

fn arch() -> &'static str {
    if cfg!(target_arch = "x86_64") || cfg!(target_arch = "amd64") {
        "amd64"
    } else if cfg!(target_arch = "i686") || cfg!(target_arch = "i386") || cfg!(target_arch = "386")
    {
        "386"
    } else if cfg!(target_arch = "armv6l") || cfg!(target_arch = "armv7l") {
        "armv6l"
    } else if cfg!(target_arch = "aarch64") || cfg!(target_arch = "arm64") {
        "arm64"
    } else {
        &ARCH
    }
}
