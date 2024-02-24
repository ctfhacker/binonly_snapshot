#![feature(absolute_path)]

//! Take a snapshot of a given binary

use clap::{Parser, ValueEnum};

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const DOCKERFILE: &str = r#"
###################################################
#### Ubuntu root FS
FROM ubuntu:jammy as base
RUN apt-get update -q \
  && apt-get install -q -y build-essential clang gdb python3 $PACKAGES$ \
  && apt-get clean -y \
  && rm -rf /var/lib/apt/lists/*

# Copy binary into the root
COPY $BINARY$ /opt/
$TRUNCATE$
$FILES$

###################################################
FROM ctfhacker/snapchange_snapshot

COPY --from=base / "$SNAPSHOT_INPUT"

ENV SNAPSHOT_ENTRYPOINT=/opt/$BINARYNAME$
"#;

const CARGO_TOML: &str = include_str!("../files/Cargo.toml");
const BUILD_RS: &str = include_str!("../files/build.rs");
const RESET_SH: &str = include_str!("../files/reset.sh");
const MAIN_RS: &str = include_str!("../files/src/main.rs");
const FUZZER_RS: &str = include_str!("../files/src/fuzzer.rs");
const LIBFUZZER_RS: &str = include_str!("../files/src/fuzzer.rs.libfuzzer");
const CONSTANTS_RS: &str = include_str!("../files/src/constants.rs");

/// The type of images available to take a snapshot with
#[derive(ValueEnum, Debug, Copy, Clone, PartialEq, Eq)]
enum ImgType {
    /// Use a disk image type
    Disk,

    /// Use an initramfs image type
    Initramfs,
}

/// Replay a given snapshot in KVM
#[derive(Parser, Debug)]
pub struct CommandLineArgs {
    /// The function to break and take a snapshot at
    #[clap(long, short)]
    pub function: Option<String>,

    /// The type of image to use to take the snapshot
    #[clap(long)]
    image_type: Option<ImgType>,

    /// This binary is a libfuzzer binary and take a snapshot at `LLVMFuzzerTestOneInput`
    #[clap(long, default_value_t = false)]
    pub libfuzzer: bool,

    /// Additional packages to install into the base image of the target
    #[clap(long)]
    pub packages: Option<Vec<String>>,

    /// The size of the input file (in bytes) to generate via `truncate`
    #[clap(long, value_parser = parse_size)]
    pub input_file_size: Option<u64>,

    /// The binary to take a snapshot of
    pub binary: PathBuf,

    /// Optional arguments passed to the binary to snapshot. @@ to use the default input file.
    pub arguments: Option<String>,
}

fn main() -> Result<(), std::io::Error> {
    const TRUNCATE_FILE_NAME: &str = "/opt/truncated_input_file";

    let args = CommandLineArgs::parse();

    // let binary = std::path::absolute(args.binary)?;
    let binary_name = args.binary.as_path().file_name().unwrap().to_str().unwrap();

    // Flag for if there is an input file as an argument
    let mut has_file = false;

    let mut arguments = Vec::new();
    let mut files = Vec::new();
    if let Some(curr_arguments) = args.arguments {
        for argument in curr_arguments.split(' ') {
            // If this argument is a file, copy the file into the docker for snapshotting
            if let Ok(test_file) = std::path::absolute(argument) {
                if test_file.exists() && !argument.starts_with("/dev") {
                    files.push(format!("COPY {test_file:?} /opt"));
                    let filename = test_file.file_name().unwrap();
                    arguments.push(format!("/opt/{}", filename.to_str().unwrap()));
                    continue;
                }
            }

            // Replace @@ with the input file name
            if argument == "@@" {
                arguments.push(TRUNCATE_FILE_NAME.to_string());
                has_file = true;
                continue;
            }

            // Default to using the argument as given
            arguments.push(argument.to_string());
        }
    }

    let truncate = if has_file {
        let size = args.input_file_size.unwrap_or(32 * 1024);
        format!("RUN truncate -s {size} {TRUNCATE_FILE_NAME}")
    } else {
        "".to_string()
    };

    let mut dockerfile = DOCKERFILE
        .to_string()
        .replace("$BINARY$", args.binary.to_str().unwrap())
        .replace("$BINARYNAME$", binary_name)
        .replace(
            "$PACKAGES$",
            &args.packages.unwrap_or_else(|| Vec::new()).join(" "),
        )
        .replace("$TRUNCATE$", &truncate)
        .replace("$FILES$", &files.join("\n"));

    // Default to taking a snapshot at `main`
    let function = args.function.unwrap_or_else(|| {
        if args.libfuzzer {
            "LLVMFuzzerTestOneInput".to_string()
        } else {
            "main".to_string()
        }
    });
    dockerfile.push_str(&format!("ENV SNAPSHOT_FUNCTION={}\n", &function));

    // Default to using an initramfs image type
    let imgtype = args.image_type.unwrap_or(ImgType::Initramfs);
    let imgtype = format!("{imgtype:?}").to_lowercase();
    dockerfile.push_str(&format!("ENV SNAPSHOT_IMGTYPE={imgtype}\n"));

    // Add the arguments if there are any for this binary
    if !arguments.is_empty() {
        let arguments = arguments.join(" ");
        dockerfile.push_str(&format!(
            "ENV SNAPSHOT_ENTRYPOINT_ARGUMENTS=\"{arguments}\"\n"
        ));
    }

    // Write the docker file for this configuration
    println!("{dockerfile}");
    let filename = format!("Dockerfile.{binary_name}");
    std::fs::write(&filename, &dockerfile).unwrap();

    let docker_tag = format!("binonly_snapchange_{binary_name}");

    // docker build -f ./Dockerfile -t harness6 .
    let mut build_cmd = Command::new("docker")
        .args(["build", "-f", &filename, "-t", &docker_tag, "."])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;

    build_cmd.wait().unwrap();

    let outdir = format!("snapchange_{binary_name}");
    let outdir = Path::new(&outdir);
    let volume = format!(
        "{}:/snapshot/",
        std::path::absolute(outdir.join("snapshot"))?
            .to_str()
            .unwrap()
    );

    // Move the output directory if it exists already
    let outdir = outdir.to_str().unwrap();
    if Path::new(outdir).exists() {
        let mut new_dir = format!("{outdir}.old");
        for count in 1..64 * 1024 {
            new_dir = format!("{outdir}.old{count}");
            if !Path::new(&new_dir).exists() {
                break;
            }
        }

        if Path::new(&new_dir).exists() {
            panic!("Too many old dirs currently.. Cannot move the output directory {outdir}");
        }

        std::fs::rename(outdir, &new_dir).unwrap();
    }

    // Create the output directory
    std::fs::create_dir_all(&outdir)?;

    // Write the static files for the snapshot
    let outdir = Path::new(&outdir);
    std::fs::create_dir_all(outdir.join("src"))?;
    std::fs::write(outdir.join("Cargo.toml"), CARGO_TOML)?;
    std::fs::write(outdir.join("build.rs"), BUILD_RS)?;
    std::fs::write(outdir.join("reset.sh"), RESET_SH)?;
    std::fs::write(outdir.join("src").join("main.rs"), MAIN_RS)?;

    let fuzzer_file = if function == "LLVMFuzzerTestOneInput" {
        LIBFUZZER_RS
    } else {
        FUZZER_RS
    };

    std::fs::write(outdir.join("src").join("fuzzer.rs"), fuzzer_file)?;
    std::fs::write(outdir.join("src").join("constants.rs"), CONSTANTS_RS)?;

    // docker run -i \
    //     -v $(realpath -m ./snapshot):/snapshot/ \
    //     harness6
    let mut run_cmd = Command::new("docker")
        .args(["run", "-i", "-v", &volume, &docker_tag])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;

    run_cmd.wait().unwrap();

    Ok(())
}

enum Size {
    Kilobyte,
    Megabyte,
    Gigabyte,
}

pub fn parse_size(mut input: &str) -> Result<u64, std::num::ParseIntError> {
    let format = if input.ends_with("k") || input.ends_with("K") {
        Some(Size::Kilobyte)
    } else if input.ends_with("m") || input.ends_with("M") {
        Some(Size::Megabyte)
    } else if input.ends_with("g") || input.ends_with("G") {
        Some(Size::Gigabyte)
    } else {
        None
    };

    // Remove the last byte if it is a format byte
    if format.is_some() {
        let mut chars = input.chars();
        let _last_char = chars.next_back();
        input = chars.as_str();
    }

    let num = input.parse::<u64>()?;
    Ok(match format {
        Some(Size::Kilobyte) => num.checked_mul(1024).expect("Too large size"),
        Some(Size::Megabyte) => num.checked_mul(1024 * 1024).expect("Too large size"),
        Some(Size::Gigabyte) => num.checked_mul(1024 * 1024 * 1024).expect("Too large size"),
        None => num,
    })
}
