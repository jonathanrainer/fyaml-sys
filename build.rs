#![allow(clippy::let_underscore_untyped, clippy::uninlined_format_args)]

use git2::{DescribeFormatOptions, DescribeOptions, Repository};
use regex::Regex;
use semver::Version;
use std::env;
use std::error::Error;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{self, Command};

fn generate_new_version(last_version: &str, commit_sha: &str) -> Result<String, String> {
    // sanitize version string (add a patch number if missing)
    let mut version = last_version.to_string();
    let parts = last_version.split('.').collect::<Vec<&str>>();
    // add ".0" one or 2 times
    for _ in 0..(3 - parts.len()) {
        version.push_str(".0");
    }

    let mut version = Version::parse(&version)
        .map_err(|e| format!("Failed to parse {:?}: {}", last_version, e))?;

    version.patch += 1;

    version.pre = semver::Prerelease::new("alpha")
        .map_err(|e| format!("Failed to add pre-release: {}", e))?;
    version.build = semver::BuildMetadata::new(commit_sha)
        .map_err(|e| format!("Failed to add build-metadata: {}", e))?;

    Ok(version.to_string())
}

fn latest_git_tag(repo_path: &str) -> Result<(String, Option<String>), Box<dyn Error>> {
    let repo = Repository::open(repo_path)?;

    let mut describe_options = DescribeOptions::new();
    describe_options.describe_tags();

    let mut format_options = DescribeFormatOptions::new();
    format_options.abbreviated_size(7); // Full tag name

    let describe = repo.describe(&describe_options)?;
    let result = describe.format(Some(&format_options))?;
    let result = result.trim_start_matches('v');
    // `git describe` formats post-tag commits as `<tag>-<N>-g<sha>`.  The
    // tag itself may contain dashes (e.g. `1.0.0-alpha7`), so we detect
    // the post-tag suffix by looking at the last segment: a `g`-prefixed
    // abbreviated SHA. Without that suffix, treat the whole string as
    // the tag and report no extra commits.
    let parts: Vec<&str> = result.split('-').collect();
    let last = parts.last().copied().unwrap_or("");
    let has_post_tag_suffix = parts.len() >= 3
        && last.starts_with('g')
        && last[1..].chars().all(|c| c.is_ascii_hexdigit());
    if has_post_tag_suffix {
        // tag is everything except the trailing `-<N>-g<sha>` pair.
        let tag = parts[..parts.len() - 2].join("-");
        let sha = last.trim_start_matches('g').to_string();
        Ok((tag, Some(sha)))
    } else {
        Ok((parts.join("-"), None))
    }
}

/// Scan every `.h` under `header_root` for declarations of `fy_*` functions.
/// libfyaml v1.0 split the public API across many subheaders under
/// `include/libfyaml/`, so the single umbrella header is no longer enough
/// to discover the va_list functions we need to blocklist.
fn function_names(header_root: &Path) -> Vec<String> {
    let re = Regex::new(r"^([a-zA-Z].*\s+)?(fy_[a-zA-Z_][a-zA-Z0-9_]+)\([^)]").unwrap();
    let mut out = Vec::new();
    let mut stack = vec![header_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for ent in entries.flatten() {
            let path = ent.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("h") {
                continue;
            }
            let f = match File::open(&path) {
                Ok(f) => f,
                Err(_) => continue,
            };
            for line in BufReader::new(f).lines().map_while(Result::ok) {
                if let Some(cap) = re.captures(&line) {
                    if let Some(m) = cap.get(2) {
                        out.push(m.as_str().to_string());
                    }
                }
            }
        }
    }
    out
}

/// Apply every `*.patch` under `patch_dir` to the vendored `repo` tree.
///
/// Patches are unified diffs with `a/` `b/` prefixes (as produced by
/// `git diff`). Each patch is applied with `git apply -p1` from inside `repo`.
///
/// Idempotency is content-based and does NOT require git for the common case:
/// before doing anything, we check whether the patch's first added line is
/// already present in the target file, and if so we skip. This matters because
/// a crates.io tarball ships the submodule sources with the patch ALREADY
/// applied (the `cargo package` run captured the patched tree), and consumers
/// installing from crates.io may not have `git` available at all. Only when a
/// patch is genuinely not yet applied do we shell out to `git apply` — which is
/// the from-git-source build, where git is guaranteed present.
///
/// A patch that is neither applied nor applies cleanly is a hard error: we must
/// never silently compile unpatched sources.
fn apply_patches(patch_dir: &Path, repo: &Path) {
    if !patch_dir.is_dir() {
        return;
    }
    let mut patches: Vec<PathBuf> = fs::read_dir(patch_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("patch"))
        .collect();
    patches.sort(); // deterministic application order

    for patch in &patches {
        println!("cargo:rerun-if-changed={}", patch.display());
        let text = fs::read_to_string(patch)
            .unwrap_or_else(|e| panic!("cannot read patch {}: {}", patch.display(), e));
        let (target_rel, sentinel) = patch_target_and_sentinel(&text)
            .unwrap_or_else(|| panic!("malformed patch (no +++/added line): {}", patch.display()));
        let target = repo.join(&target_rel);

        // Already applied? (sentinel present in target) -> skip. No git needed.
        if let Ok(current) = fs::read_to_string(&target) {
            if current.contains(&sentinel) {
                continue;
            }
        }

        // Not applied: shell out to git apply (from-git-source build path).
        let abs = fs::canonicalize(patch)
            .unwrap_or_else(|e| panic!("cannot resolve patch {}: {}", patch.display(), e));
        let applied = Command::new("git")
            .current_dir(repo)
            .args(["apply", "-p1"])
            .arg(&abs)
            .status();
        match applied {
            Ok(s) if s.success() => {}
            other => panic!(
                "failed to apply patch {} to {} (git apply result: {:?}); \
                 the pinned submodule may have drifted",
                patch.display(),
                repo.display(),
                other
            ),
        }
    }
}

/// Parse a unified diff: return the target path (from the `+++ b/<path>` line,
/// stripped of the `b/` prefix) and a distinctive added line to use as an
/// "already applied" sentinel.
///
/// The sentinel is the LONGEST genuinely-added line (a `+` line that is not the
/// `+++` header), trimmed. The longest added line is the least likely to occur
/// by coincidence in the pristine file — picking the *first* added line is
/// unsafe because patches often begin with a generic line (`/*`, a blank line,
/// `#endif`) that appears many times in the target.
fn patch_target_and_sentinel(text: &str) -> Option<(PathBuf, String)> {
    let mut target: Option<PathBuf> = None;
    let mut sentinel: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            let path = rest.trim();
            let path = path.strip_prefix("b/").unwrap_or(path);
            target = Some(PathBuf::from(path));
        } else if line.starts_with('+') && !line.starts_with("+++") && line.len() > 1 {
            let body = line[1..].trim();
            if !body.is_empty() {
                let longer = sentinel.as_ref().is_none_or(|s| body.len() > s.len());
                if longer {
                    sentinel = Some(body.to_string());
                }
            }
        }
    }
    Some((target?, sentinel?))
}

fn main() {
    let windows = env::var_os("CARGO_CFG_WINDOWS").is_some();
    if windows {
        eprintln!("libfyaml is not supported on Windows.");
        eprintln!("See https://github.com/pantoniou/libfyaml/issues/10");
        process::exit(1);
    }

    let header = "libfyaml/include/libfyaml.h";
    println!("cargo:rerun-if-changed={}", header);
    println!("cargo:rerun-if-changed=build.rs");

    if let Ok(false) = Path::new(header).try_exists() {
        let _ = Command::new("git")
            .args(["submodule", "update", "--init", "libfyaml"])
            .status();
    }

    // --- vendored patches ------------------------------------------------
    //
    // The `libfyaml` submodule is pinned to a pristine upstream tag. Local
    // fixes we carry until they land upstream live as `*.patch` files under
    // `patches/` and are applied here, against the submodule tree, before
    // anything is compiled. Applied idempotently so repeated builds (and the
    // release `pkg` clone) are safe.
    apply_patches(Path::new("patches"), Path::new("libfyaml"));

    // --- bindings --------------------------------------------------------
    //
    // libfyaml v1.0 split the umbrella `<libfyaml.h>` into many subheaders
    // under `<libfyaml/...>`. Bindgen invokes libclang, which needs the
    // include path to resolve those `#include <libfyaml/libfyaml-util.h>`
    // lines. The original v0.9 build.rs relied on the monolithic header
    // and passed no clang args at all.
    let mut bindings = bindgen::builder()
        .header(header)
        .clang_arg("-Ilibfyaml/include")
        .allowlist_recursively(false)
        .allowlist_function("fy_.*")
        .allowlist_type("fy_.*");

    // Variadic functions that use `va_list`.
    // Blocked on https://github.com/rust-lang/rust/issues/44930.
    let all_function_names = function_names(Path::new("libfyaml/include"));

    // Variadic helpers in libfyaml come in `..._v<noun>` form (e.g.
    // `fy_diag_vprintf`, `fy_node_set_vanchorf`, `fy_emit_event_vcreate`).
    // The pattern matches the `v` immediately before a recognised noun.
    let re = Regex::new(
        r"_v(report|log|printf|buildf|scanf|anchorf|log|event|scalarf|create|diag|dump|printfv)\w*$",
    )
    .unwrap();
    let mut blocked: Vec<String> = all_function_names
        .into_iter()
        .filter(|name| re.is_match(name))
        .collect();
    blocked.sort();
    blocked.dedup();
    for function_name in blocked {
        bindings = bindings.blocklist_function(function_name);
    }

    let bindings = bindings
        .prepend_enum_name(false)
        .generate_comments(false)
        .formatter(bindgen::Formatter::Prettyplease)
        .generate()
        .unwrap();

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    bindings.write_to_file(out_dir.join("bindings.rs")).unwrap();

    // --- version --------------------------------------------------------
    let version = match latest_git_tag("libfyaml") {
        Ok((version, Some(commit))) => generate_new_version(&version, &commit).unwrap(),
        Ok((version, None)) => version,
        Err(_) => {
            // Not a git repo (e.g., installed from crates.io).
            let pkg_version = env::var("CARGO_PKG_VERSION").unwrap_or_default();
            pkg_version
                .split("+fy")
                .nth(1)
                .unwrap_or("0.9.3")
                .to_string()
        }
    };

    // --- main libfyaml C build -----------------------------------------
    //
    // Single source of truth: the list of libfyaml src subdirectories
    // we compile into the static lib. New top-level src dirs upstream
    // are rare and structural — adding/removing one here is the *only*
    // expected maintenance touchpoint. Within a listed dir, ALL `*.c`
    // files are auto-discovered by glob; any file added or removed
    // upstream is picked up with no edit, unless it's in the explicit
    // EXCLUDED_FILES set below (kept tiny and rationalised per entry).
    let src_dirs: &[&str] = &[
        "lib",
        "util",
        "xxhash",
        "thread",
        "allocator",
        "generic",
        "reflection",
        "blake3",
    ];

    // Bare-filename exclusions, applied to every globbed `.c` regardless of
    // directory. Names are unique enough across the libfyaml tree that
    // filename-only matching is safe; if upstream ever introduces a clash
    // we'll catch it via the "set must match expectations" check below.
    //
    // Rationale per entry:
    //   - fy-clang-backend.c   : requires libclang; we build with HAVE_LIBCLANG=0.
    //   - blake3_avx2/avx512/neon/sse2/sse41.c : per-file SIMD compiler flags
    //                            we don't set; portable kernel covers correctness.
    //   - blake3.c, blake3_portable.c : compiled separately below with
    //                            HASHER_SUFFIX=portable / SIMD_DEGREE=1 (mirrors
    //                            CMake's `b3portable` OBJECT library); must NOT
    //                            be in the main TU set.
    let excluded_files: &[&str] = &[
        "blake3.c",
        "blake3_avx2.c",
        "blake3_avx512.c",
        "blake3_neon.c",
        "blake3_portable.c",
        "blake3_sse2.c",
        "blake3_sse41.c",
        "fy-clang-backend.c",
    ];
    let excluded: std::collections::HashSet<&str> = excluded_files.iter().copied().collect();

    // Build the absolute include-dir list from the same `src_dirs` —
    // one source of truth, no parallel hand-maintained array.
    let mut include_dirs: Vec<PathBuf> = vec![PathBuf::from("libfyaml/include")];
    for d in src_dirs {
        let p = PathBuf::from("libfyaml/src").join(d);
        if p.is_dir() {
            include_dirs.push(p);
        }
    }

    // Glob each src dir for `.c` files, filter exclusions, sort for
    // determinism. Skip dirs that don't exist (defensive — lets future
    // restructurings degrade to "missing sources" rather than a panic).
    let mut lib_srcs: Vec<PathBuf> = Vec::new();
    for d in src_dirs {
        let dir = PathBuf::from("libfyaml/src").join(d);
        if !dir.is_dir() {
            continue;
        }
        // Re-run if files appear/disappear in this directory.
        println!("cargo:rerun-if-changed={}", dir.display());
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for ent in entries.flatten() {
            let path = ent.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("c") {
                continue;
            }
            let fname = match path.file_name().and_then(|s| s.to_str()) {
                Some(f) => f,
                None => continue,
            };
            if excluded.contains(fname) {
                continue;
            }
            lib_srcs.push(path);
        }
    }
    lib_srcs.sort();

    for p in &lib_srcs {
        println!("cargo:rerun-if-changed={}", p.display());
    }

    let mut build = cc::Build::new();
    build.std("gnu11"); // CMake prefers gnu2x/c2x, gnu11 is the documented fallback
    for d in &include_dirs {
        build.include(d);
    }
    for p in &lib_srcs {
        build.file(p);
    }
    build.flag_if_supported("-Wno-type-limits");
    build.flag_if_supported("-Wno-unused-but-set-parameter");
    build.flag_if_supported("-Wno-unused-parameter");
    build.flag_if_supported("-Wno-sign-compare");
    build.flag_if_supported("-Wno-missing-field-initializers");
    build.flag_if_supported("-Wno-implicit-fallthrough");
    // No `HAVE_CONFIG_H`: the libfyaml sources include `config.h` only under
    // `#ifdef HAVE_CONFIG_H`. By leaving it undefined we skip the generated
    // header entirely and let every feature guard fall back to its portable
    // path (no `mremap`/`qsort_r` specialisation, no SIMD BLAKE3). This keeps
    // the build host-agnostic — no Linux/glibc-specific assumptions baked in.
    build.define("_GNU_SOURCE", None);
    build.define("__STDC_WANT_LIB_EXT2__", "1");
    build.define("HAVE_STATEMENT_EXPRESSIONS", None);
    build.define("HAVE_GENERIC", None);
    build.define("HAVE_REFLECTION", None);
    build.define("VERSION", format!("{:?}", version).as_str());
    build.compile("fyaml");

    // --- BLAKE3 portable kernel ----------------------------------------
    //
    // CMake compiles `blake3_portable.c` and `blake3.c` as a separate
    // OBJECT library (`b3portable`) with HASHER_SUFFIX=portable /
    // SIMD_DEGREE=1, because `blake3.c` token-pastes those defines into
    // its function names. We mirror that with a second cc::Build so we
    // don't pollute the main TU defines.
    let b3_files = ["src/blake3/blake3.c", "src/blake3/blake3_portable.c"];
    for s in &b3_files {
        println!("cargo:rerun-if-changed=libfyaml/{}", s);
    }
    let mut b3 = cc::Build::new();
    b3.std("gnu11");
    b3.include("libfyaml/include")
        .include("libfyaml/src/util")
        .include("libfyaml/src/thread")
        .include("libfyaml/src/blake3");
    for s in &b3_files {
        b3.file(PathBuf::from("libfyaml").join(s));
    }
    b3.flag_if_supported("-Wno-type-limits");
    b3.flag_if_supported("-Wno-unused-parameter");
    b3.flag_if_supported("-Wno-sign-compare");
    b3.flag_if_supported("-Wno-unused-but-set-parameter");
    b3.define("_GNU_SOURCE", None);
    b3.define("HASHER_SUFFIX", "portable");
    b3.define("SIMD_DEGREE", "1");
    b3.compile("fyaml_blake3_portable");
}
