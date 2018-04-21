/* CONFIGURATION */

// add other android system headers to this list as necessary
static INCLUDE_SYS_H: [&str; 1] = ["jni.h"];

static RUST_SRC_DIR: &str = "src";
static ANDROID_BASE_DIR: &str = "app";

// =========================================================
// This script is portable; copy and paste it into your own
// build files at will.

extern crate bindgen;
extern crate rust_swig;
extern crate syntex;
extern crate walkdir;

use std::process::{Command, Stdio};
use std::{env, fmt};
use std::io::prelude::*;
use std::fs::File;
use std::path::{Path, PathBuf};

use bindgen::RustTarget;
use rust_swig::{JavaConfig, LanguageConfig};
use walkdir::WalkDir;

fn main() {
    // don't simplify this to if the target contains the substring "android" --
    // these lines also serve as a guard so only true android triples receive
    // JNI generation.
    let target = env::var("TARGET").unwrap();
    if ["aarch64-linux-android",
        "arm-linux-androideabi",
        "i686-linux-android",
        "x86_64-linux-android"]
        .contains(&target.as_str()) {
            gen_for_android();
        }
}

fn gen_for_android() {
    let target = env::var("TARGET").unwrap();
    
    let include_dirs =
        get_gcc_system_include_dirs(&target).expect("Can't get NDK's system include dirs");

    let include_headers: Vec<_> = INCLUDE_SYS_H.into_iter()
        .map(|h| search_file_in_directory(&include_dirs, h)
             .expect(format!("Could not find header {}", h).as_ref()))
        .collect();

    let src_dir = Path::new(RUST_SRC_DIR);
    let out_dir = env::var("OUT_DIR").unwrap();

    gen_binding(
        &target,
        &include_dirs,
        &include_headers,
        &Path::new(&out_dir).join("android_c_headers.rs"),
    ).unwrap();

    // Find files ending in .rs.in and expand them with SWIG
    for _entry in WalkDir::new(RUST_SRC_DIR) {
        let entry = _entry.expect("Error walking sources.");
        if entry.path().is_dir() || !entry.path().to_string_lossy().ends_with(".rs.in") {
            continue;
        }

        println!("Found SWIG specification: {}", entry.path().display());
        let swigf = entry.path().strip_prefix("src").unwrap();
        
        rust_swig_expand(
            &src_dir,
            &swigf,
            Path::new(&out_dir),
        ).unwrap();

        write_include_file(&src_dir, swigf).expect("Failed to write include file.");
    }

    // Hook up cargo reruns
    println!("cargo:rerun-if-changed={}", RUST_SRC_DIR);
    for dir in &include_dirs {println!("cargo:rerun-if-changed={}", dir.display());}
    //if the generated files were deleted (e.g by gradle -q clean), regenerate them
    println!("cargo:rerun-if-changed={}", out_dir);
}

fn get_gcc_system_include_dirs(target: &str) -> Result<Vec<PathBuf>, String> {
    let gcc_cmd = match env::var("RUSTC_LINKER") {
        Ok(path) => path,
        Err(_) => target.to_owned() + "-gcc"
    };

    assert!(Path::new(&gcc_cmd).exists(),
            "RUSTC_LINKER not set -- Update your version of Cargo
or merge PR#5394 from github.com/rust-lang/cargo.");

    println!("Using Android gcc from '{}'", gcc_cmd);

    let gcc_process = Command::new(&gcc_cmd)
        .args(&["-v", "-x", "c", "-E", "-" /*, &("--sysroot=".to_owned() + &sysroot)*/])
        .stderr(Stdio::piped())
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .spawn()
        .map_err(|err| err.to_string())?;

    gcc_process
        .stdin
        .ok_or(format!("Cannot get stdin of {}", gcc_cmd).as_str())?
        .write_all("\n".as_bytes())
        .map_err(|err| err.to_string())?;

    let mut gcc_output = String::new();
    gcc_process
        .stderr
        .ok_or(format!("Cannot get stderr of {}", gcc_cmd).as_str())?
        .read_to_string(&mut gcc_output)
        .map_err(|err| err.to_string())?;

    const BEGIN_PAT: &'static str = "\n#include <...> search starts here:\n";
    const END_PAT: &'static str = "\nEnd of search list.\n";
    let start_includes = gcc_output
        .find(BEGIN_PAT)
        .ok_or(format!("No '{}' in output from {}", BEGIN_PAT, gcc_cmd).as_str())?
        + BEGIN_PAT.len();
    let end_includes = (&gcc_output[start_includes..])
        .find(END_PAT)
        .ok_or(format!("No '{}' in output from {}", END_PAT, gcc_cmd).as_str())?
        + start_includes;

    Ok((&gcc_output[start_includes..end_includes])
       .split('\n')
       .map(|s| PathBuf::from(s.trim().to_string()))
       .collect())
}

fn search_file_in_directory<P>(dirs: &[P], file: &str) -> Result<PathBuf, ()>
where
    P: AsRef<Path>,
{
    for dir in dirs {
        let file_path = dir.as_ref().join(file);
        if file_path.exists() && file_path.is_file() {
            return Ok(file_path);
        }
    }
    Err(())
}

fn gen_binding<P1, P2>(
    target: &str,
    include_dirs: &[P1],
    c_headers: &[P2],
    output_rust: &Path,
) -> Result<(), String>
where
    P1: AsRef<Path> + fmt::Debug,
    P2: AsRef<Path> + fmt::Debug,
{
    assert!(!c_headers.is_empty());
    let c_file_path = &c_headers[0];

    let mut bindings: bindgen::Builder =
        bindgen::builder().header(c_file_path.as_ref().to_str().unwrap());
    bindings = include_dirs.iter().fold(bindings, |acc, x| {
        acc.clang_arg("-I".to_string() + x.as_ref().to_str().unwrap())
    });
    println!("Generate binding for {:?}", c_headers);
    bindings = bindings
        .rust_target(RustTarget::Stable_1_19)
    //long double not supported yet, see https://github.com/servo/rust-bindgen/issues/550
        .blacklist_type("max_align_t");
    bindings = if target.contains("windows") {
        //see https://github.com/servo/rust-bindgen/issues/578
        bindings.trust_clang_mangling(false)
    } else {
        bindings
    };
    bindings = c_headers[1..].iter().fold(
        Ok(bindings),
        |acc: Result<bindgen::Builder, String>, header| {
            let c_file_path = header;
            let c_file_str = c_file_path
                .as_ref()
                .to_str()
                .ok_or_else(|| format!("Invalid unicode in path to {:?}", c_file_path.as_ref()))?;
            Ok(acc.unwrap().clang_arg("-include").clang_arg(c_file_str))
        },
    )?;

    let generated_bindings = bindings
    //        .clang_arg(format!("-target {}", target))
        .generate()
        .map_err(|_| "Failed to generate bindings".to_string())?;
    generated_bindings
        .write_to_file(output_rust)
        .map_err(|err| err.to_string())?;

    Ok(())
}

fn rust_swig_expand(source_dir: &Path, file: &Path, out_dir: &Path) -> Result<(), String> {
    let app_id = env::var("ANDROID_APPLICATION_ID")
        .expect("You must set ANDROID_APPLICATION_ID to the package name of your android application.");
    
    let mut registry = syntex::Registry::new();
    let swig_gen = rust_swig::Generator::new(LanguageConfig::JavaConfig(
        JavaConfig::new(
            Path::new(ANDROID_BASE_DIR)
                .join("src")
                .join("main")
                .join("java")
                .join(app_id.replace(".", "/")),
            app_id.to_owned() // not sure why it requires ownership...
        ).use_null_annotation("android.support.annotation.NonNull".into()),
    ));
    swig_gen.register(&mut registry);

    let out_file = out_dir.join(Path::new(file.parent().unwrap_or(Path::new(".")))
                                .join(file.file_stem().expect("Got invalid file (no filename)")));
    
    registry
        .expand(app_id.as_ref(), source_dir.join(file), out_file)
        .map_err(|err| format!("Rust-SWIG expansion failed: {}", err))
}

fn write_include_file(source_dir: &Path, swig_file: &Path) -> std::io::Result<()> {
    let rs_rel_file = Path::new(swig_file.parent().unwrap_or(Path::new(".")))
        .join(swig_file.file_stem().expect("Got invalid file (no filename)"));
    let rs_path = source_dir.join(&rs_rel_file);

    if rs_path.exists() {
        println!("Not writing {} because it exists", rs_path.display());
        return Ok(());
    }

    let mut rs_file = File::create(rs_path)?;
    rs_file.write_all(format!("include!(concat!(env!(\"OUT_DIR\"), \"/{}\"",
                              rs_rel_file.to_string_lossy()).as_bytes())?;

    return Ok(());
}
