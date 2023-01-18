use std::fs::File;
use std::io::{Cursor, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::{env, fs};

use bytes::Bytes;
use convert_case::{Case, Casing};
use flate2::read::GzDecoder;
use reqwest::IntoUrl;

const BASE_URL: &str = "https://milli-benchmarks.fra1.digitaloceanspaces.com/datasets";

const DATASET_SONGS: (&str, &str) = ("smol-songs", "csv");
const DATASET_SONGS_1_2: (&str, &str) = ("smol-songs-1_2", "csv");
const DATASET_SONGS_3_4: (&str, &str) = ("smol-songs-3_4", "csv");
const DATASET_SONGS_4_4: (&str, &str) = ("smol-songs-4_4", "csv");
const DATASET_WIKI: (&str, &str) = ("smol-wiki-articles", "csv");
const DATASET_WIKI_1_2: (&str, &str) = ("smol-wiki-articles-1_2", "csv");
const DATASET_WIKI_3_4: (&str, &str) = ("smol-wiki-articles-3_4", "csv");
const DATASET_WIKI_4_4: (&str, &str) = ("smol-wiki-articles-4_4", "csv");
const DATASET_MOVIES: (&str, &str) = ("movies", "json");
const DATASET_MOVIES_1_2: (&str, &str) = ("movies-1_2", "json");
const DATASET_MOVIES_3_4: (&str, &str) = ("movies-3_4", "json");
const DATASET_MOVIES_4_4: (&str, &str) = ("movies-4_4", "json");
const DATASET_NESTED_MOVIES: (&str, &str) = ("nested_movies", "json");
const DATASET_GEO: (&str, &str) = ("smol-all-countries", "jsonl");

const ALL_DATASETS: &[(&str, &str)] = &[
    DATASET_SONGS,
    DATASET_SONGS_1_2,
    DATASET_SONGS_3_4,
    DATASET_SONGS_4_4,
    DATASET_WIKI,
    DATASET_WIKI_1_2,
    DATASET_WIKI_3_4,
    DATASET_WIKI_4_4,
    DATASET_MOVIES,
    DATASET_MOVIES_1_2,
    DATASET_MOVIES_3_4,
    DATASET_MOVIES_4_4,
    DATASET_NESTED_MOVIES,
    DATASET_GEO,
];

/// The name of the environment variable used to select the path
/// of the directory containing the datasets
const BASE_DATASETS_PATH_KEY: &str = "MILLI_BENCH_DATASETS_PATH";

fn main() -> anyhow::Result<()> {
    let out_dir = PathBuf::from(env::var(BASE_DATASETS_PATH_KEY).unwrap_or(env::var("OUT_DIR")?));

    let benches_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?).join("benches");
    let mut manifest_paths_file = File::create(benches_dir.join("datasets_paths.rs"))?;
    write!(
        manifest_paths_file,
        r#"//! This file is generated by the build script.
//! Do not modify by hand, use the build.rs file.
#![allow(dead_code)]
"#
    )?;
    writeln!(manifest_paths_file)?;

    for (dataset, extension) in ALL_DATASETS {
        let out_path = out_dir.join(dataset);
        let out_file = out_path.with_extension(extension);

        writeln!(
            &mut manifest_paths_file,
            r#"pub const {}: &str = {:?};"#,
            dataset.to_case(Case::ScreamingSnake),
            out_file.display(),
        )?;

        if out_file.exists() {
            eprintln!(
                "The dataset {} already exists on the file system and will not be downloaded again",
                out_path.display(),
            );
            continue;
        }
        let url = format!("{}/{}.{}.gz", BASE_URL, dataset, extension);
        eprintln!("downloading: {}", url);
        let bytes = retry(|| download_dataset(url.clone()), 10)?;
        eprintln!("{} downloaded successfully", url);
        eprintln!("uncompressing in {}", out_file.display());
        uncompress_in_file(bytes, &out_file)?;
    }

    Ok(())
}

fn retry<Ok, Err>(fun: impl Fn() -> Result<Ok, Err>, times: usize) -> Result<Ok, Err> {
    for _ in 0..times {
        if let ok @ Ok(_) = fun() {
            return ok;
        }
    }
    fun()
}

fn download_dataset<U: IntoUrl>(url: U) -> anyhow::Result<Cursor<Bytes>> {
    let bytes =
        reqwest::blocking::Client::builder().timeout(None).build()?.get(url).send()?.bytes()?;
    Ok(Cursor::new(bytes))
}

fn uncompress_in_file<R: Read + Seek, P: AsRef<Path>>(bytes: R, path: P) -> anyhow::Result<()> {
    let path = path.as_ref();
    let mut gz = GzDecoder::new(bytes);
    let mut dataset = Vec::new();
    gz.read_to_end(&mut dataset)?;

    fs::write(path, dataset)?;
    Ok(())
}