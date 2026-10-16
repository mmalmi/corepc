fn main() { download::start().unwrap(); }

#[cfg(any(docsrs, not(feature = "download")))]
mod download {
    pub(crate) fn start() -> Result<(), ()> { Ok(()) }
}

#[cfg(feature = "download")]
mod download {
    use std::fs::File;
    use std::io::{BufRead, BufReader, Read};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::str::FromStr;

    use anyhow::Context;
    use bitcoin_hashes::{sha256, Hash};

    include!("src/versions.rs");

    fn download_filename() -> String {
        if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
            if cfg!(not(feature = "23_2")) {
                return format!("bitcoin-{}-osx64.tar.gz", &VERSION);
            }
            return format!("bitcoin-{}-x86_64-apple-darwin.tar.gz", &VERSION);
        }

        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            return format!("bitcoin-{}-arm64-apple-darwin.tar.gz", &VERSION);
        }

        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            return format!("bitcoin-{}-x86_64-linux-gnu.tar.gz", &VERSION);
        }

        if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
            return format!("bitcoin-{}-aarch64-linux-gnu.tar.gz", &VERSION);
        }

        if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
            return format!("bitcoin-{}-win64.zip", &VERSION);
        }

        panic!("No download file for this os/arch");
    }

    #[allow(clippy::lines_filter_map_ok)] // clippy doesn't like the `lines` call below and the suggested fix is incorrect.
    fn get_expected_sha256(filename: &str) -> anyhow::Result<sha256::Hash> {
        let sha256sums_filename = format!("sha256/bitcoin-core-{}-SHA256SUMS", &VERSION);
        #[cfg(not(feature = "22_1"))]
        let sha256sums_filename = format!("{}.asc", sha256sums_filename);
        let file = File::open(&sha256sums_filename)
            .with_context(|| format!("cannot find {:?}", sha256sums_filename))?;
        for line in BufReader::new(file).lines().flatten() {
            let tokens: Vec<_> = line.split("  ").collect();
            if tokens.len() == 2 && filename == tokens[1] {
                return Ok(sha256::Hash::from_str(tokens[0]).unwrap());
            }
        }
        panic!(
            "Couldn't find hash for `{}` in `{}`:\n{}",
            filename,
            sha256sums_filename,
            std::fs::read_to_string(&sha256sums_filename).unwrap()
        );
    }

    fn download_dir(out_dir: &Path) -> PathBuf {
        if let Some(path) = std::env::var_os("BITCOIND_DOWNLOAD_DIR") {
            return PathBuf::from(path);
        }
        out_dir.join("bitcoin")
    }

    pub(crate) fn start() -> anyhow::Result<()> {
        if std::env::var_os("BITCOIND_SKIP_DOWNLOAD").is_some() {
            return Ok(());
        }
        let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());

        let bitcoin_exe_home = download_dir(&out_dir);
        std::fs::create_dir_all(&bitcoin_exe_home)
            .with_context(|| format!("cannot create dir {:?}", bitcoin_exe_home))?;

        let version_dir = bitcoin_exe_home.join(format!("bitcoin-{}", VERSION));
        let mut existing_filename = version_dir.join("bin");
        if cfg!(target_os = "windows") {
            existing_filename.push("bitcoind.exe");
        } else {
            existing_filename.push("bitcoind");
        }

        #[cfg(not(target_os = "windows"))]
        let cache_complete = existing_filename.exists()
            && version_dir.join("bin").join("bitcoin-cli").exists()
            && (!cfg!(feature = "30_2")
                || version_dir.join("libexec").join("bitcoin-node").exists());
        #[cfg(target_os = "windows")]
        let cache_complete = existing_filename.exists();

        if !cache_complete {
            let download_filename = download_filename();
            println!("download_filename: {}", download_filename);
            let expected_hash = get_expected_sha256(&download_filename)?;

            println!("filename:{} version:{} hash:{}", download_filename, VERSION, expected_hash);

            let (file_or_url, tarball_bytes) = match std::env::var("BITCOIND_TARBALL_FILE") {
                Err(_) => {
                    let download_endpoint = std::env::var("BITCOIND_DOWNLOAD_ENDPOINT")
                        .unwrap_or("https://bitcoincore.org/bin".to_owned());

                    let url = format!(
                        "{}/bitcoin-core-{}/{}",
                        download_endpoint, VERSION, download_filename
                    );
                    let resp = bitreq::get(&url)
                        .send()
                        .with_context(|| format!("cannot reach url {}", url))?;
                    assert_eq!(resp.status_code, 200, "url {} didn't return 200", url);

                    (url, resp.as_bytes().to_vec())
                }
                Ok(path) => {
                    let f = File::open(&path).with_context(|| {
                        format!(
                            "Cannot find {:?} specified with env var BITCOIND_TARBALL_FILE",
                            &path
                        )
                    })?;
                    let mut reader = BufReader::new(f);
                    let mut buffer = Vec::new();
                    reader.read_to_end(&mut buffer)?;
                    (path, buffer)
                }
            };

            let tarball_hash = sha256::Hash::hash(&tarball_bytes);
            assert_eq!(
                expected_hash, tarball_hash,
                "expected hash of {} is not matching",
                file_or_url
            );

            let archive_path = out_dir.join(&download_filename);
            std::fs::write(&archive_path, &tarball_bytes)?;
            let mut command = Command::new("tar");
            command.arg("-xf").arg(&archive_path).arg("-C").arg(&bitcoin_exe_home);
            if cfg!(target_os = "windows") {
                command.arg(format!("bitcoin-{}/bin/bitcoind.exe", VERSION));
            } else {
                command
                    .arg(format!("bitcoin-{}/bin/bitcoind", VERSION))
                    .arg(format!("bitcoin-{}/bin/bitcoin-cli", VERSION));
                if cfg!(feature = "30_2") {
                    command.arg(format!("bitcoin-{}/libexec/bitcoin-node", VERSION));
                }
            }
            let output = command
                .output()
                .context("failed to run tar; install tar and ensure it is in PATH")?;
            anyhow::ensure!(
                output.status.success(),
                "tar failed to extract {}: {}",
                download_filename,
                String::from_utf8_lossy(&output.stderr)
            );
            std::fs::remove_file(&archive_path)?;

            // Code signing for arm64 macOS:
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                let to_sign = [
                    version_dir.join("bin").join("bitcoind"),
                    version_dir.join("bin").join("bitcoin-cli"),
                    version_dir.join("libexec").join("bitcoin-node"),
                ];
                for binary in to_sign.iter().filter(|p| p.exists()) {
                    let signing_status =
                        Command::new("codesign").arg("-v").arg(binary).status().with_context(
                            || format!("failed to verify code signature on {:?}", binary),
                        )?;

                    if !signing_status.success() {
                        let status = Command::new("codesign")
                            .arg("-s")
                            .arg("-")
                            .arg(binary)
                            .status()
                            .with_context(|| format!("failed to sign {:?}", binary))?;
                        if !status.success() {
                            return Err(anyhow::anyhow!(
                                "codesign failed for {:?} with exit code {}",
                                binary,
                                status.code().unwrap_or(-1)
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
