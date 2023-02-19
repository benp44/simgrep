use async_recursion::async_recursion;
use camino::{Utf8Path, Utf8PathBuf};
use clap::Parser;
use colored::Colorize;
use content_inspector::{inspect, ContentType};
use futures::future::join_all;
use lazy_static::lazy_static;
use rlimit::Resource;
use std::cmp::min;
use std::io::{prelude::*, BufReader, ErrorKind};
use std::{fs::File, io::Read};
use tokio::sync::Semaphore;
use tokio::task;

lazy_static! {
    static ref OPEN_FILES_SEMAPHORE: Semaphore = {
        let process_open_files_limit = Resource::NOFILE.get().unwrap().0 as usize;
        Semaphore::new(process_open_files_limit)
    };
}

#[derive(Clone)]
struct Match {
    file_path: Utf8PathBuf,
    line_number: usize,
    line: String,
}

impl Match {
    fn format_as_result(
        &self,
        pattern: &str,
        max_path_length: Option<usize>,
        max_line_number_length: Option<usize>,
    ) -> String {
        let file_ref = format!("{}:{}", self.file_path, self.line_number);
        let pattern_replace = pattern.blue().to_string();
        let pattern_text = self.line.replace(pattern, &pattern_replace);

        let path_width = max_path_length.unwrap_or(0);
        let line_number_width = max_line_number_length.unwrap_or(0);
        let file_ref_width = path_width + line_number_width + 1; // Include colon from above

        let file_ref_value = format!("{:<file_ref_width$}│", file_ref);

        format!("{}{}", file_ref_value.red(), pattern_text)
    }
}

async fn process_entry(path: &Utf8Path, pattern: &str) -> Vec<Match> {
    if path.is_dir() {
        process_dir(path, pattern).await
    } else {
        process_file(path, pattern).await
    }
}

async fn file_is_text(path: &Utf8Path) -> Result<bool, String> {
    let permit = OPEN_FILES_SEMAPHORE
        .acquire()
        .await
        .map_err(|err| format!("error acquring permit: {err}"))?;

    let path_buf = Utf8PathBuf::from(path);

    let file_is_text = task::spawn_blocking(move || {
        let mut scan_file = File::open(&path_buf)
            .map_err(|err| format!("error opening file: {path_buf}. {err}"))?;

        let file_length = scan_file
            .metadata()
            .map_err(|err| format!("error getting file metadata: {path_buf}. {err}"))?
            .len();

        let length_to_read: usize = min(file_length, 2048)
            .try_into()
            .map_err(|err| format!("error converting file size to usize: {path_buf}. {err}"))?;

        let mut buffer = vec![0u8; length_to_read];
        scan_file
            .read_exact(&mut buffer)
            .map_err(|err| format!("error scanning file for content type: {path_buf}. {err}"))?;

        let detected_content_type = inspect(&buffer);

        let detected_as_text = match detected_content_type {
            ContentType::UTF_8
            | ContentType::UTF_8_BOM
            | ContentType::UTF_16LE
            | ContentType::UTF_16BE
            | ContentType::UTF_32LE
            | ContentType::UTF_32BE => true,
            ContentType::BINARY => false,
        };

        Result::<bool, String>::Ok(detected_as_text)
    })
    .await
    .map_err(|err| format!("error joining when scanning file: {path}. {err}"))??;

    drop(permit);

    Ok(file_is_text)
}

async fn get_matches_in_file(path: &Utf8Path, pattern: &str) -> Result<Vec<Match>, String> {
    let path_buf = Utf8PathBuf::from(path);
    let pattern = String::from(pattern);

    let permit = OPEN_FILES_SEMAPHORE
        .acquire()
        .await
        .map_err(|err| format!("error acquring permit: {err}"))?;

    let matches = task::spawn_blocking(move || {
        let file = File::open(&path_buf)
            .map_err(|err| format!("error opening file: {path_buf}. {err}"))?;

        let reader = BufReader::new(file);

        let mut matches = Vec::<Match>::new();
        for (line_number, line_result) in reader.lines().enumerate() {
            let path_buf = Utf8PathBuf::from(&path_buf);
            let pattern = String::from(&pattern);

            match line_result {
                Ok(line) => {
                    if line.contains(&pattern) {
                        matches.push(Match {
                            file_path: path_buf,
                            line_number,
                            line: line.to_string(),
                        });
                    }
                }
                Err(err) if err.kind() == ErrorKind::InvalidData => {}
                Err(err) => {
                    println!("failed to read line from file {path_buf}: {err}")
                }
            }
        }

        Result::<Vec<Match>, String>::Ok(matches)
    })
    .await
    .map_err(|err| format!("error joining when reading file: {path}. {err}"))??;

    drop(permit);

    Ok(matches)
}

async fn process_file(path: &Utf8Path, pattern: &str) -> Vec<Match> {
    let file_is_text = match file_is_text(path).await {
        Ok(file_is_text) => file_is_text,
        Err(err) => {
            eprintln!("Error detecting file type: {err}");
            false
        }
    };

    if file_is_text {
        match get_matches_in_file(path, pattern).await {
            Ok(matches) => matches,
            Err(err) => {
                eprintln!("{err}");
                vec![]
            }
        }
    } else {
        vec![]
    }
}

#[async_recursion]
async fn process_dir(path: &Utf8Path, pattern: &str) -> Vec<Match> {
    let read_dir_result = path.read_dir_utf8();

    let dir_entry = match read_dir_result {
        Ok(dir_entry) => dir_entry.collect(),
        Err(err) => {
            eprintln!("{err}");
            vec![]
        }
    };

    dir_entry
        .iter()
        .filter_map(|child_result| child_result.as_ref().err())
        .map(|err| format!("could not get child entry: {err}"))
        .for_each(|message| eprintln!("{message}"));

    let tasks = dir_entry
        .into_iter()
        .filter_map(|child_result| child_result.ok())
        .map(|child_entry| {
            let pattern = String::from(pattern);
            tokio::spawn(async move { process_entry(child_entry.path(), &pattern).await })
        });

    let all_spawn_results = join_all(tasks).await;

    all_spawn_results
        .iter()
        .filter_map(|join_result| join_result.as_ref().err())
        .for_each(|message| eprintln!("Error getting results: {message}"));

    let all_matches: Vec<Match> = all_spawn_results
        .iter()
        .filter_map(|join_result| join_result.as_ref().ok())
        .flatten()
        .cloned()
        .collect();

    all_matches
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(value_name = "pattern")]
    pattern: String,

    #[arg(default_value_t = Utf8PathBuf::from("."), value_name = "path")]
    path: Utf8PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let args = Args::parse();

    let pattern = args.pattern;
    let root_search_path = args.path.as_path();
    let matches = process_entry(root_search_path, &pattern).await;

    let max_path_length = matches.iter().map(|x| x.file_path.as_str().len()).max();
    let max_line_number_length = matches
        .iter()
        .map(|x| x.line_number.to_string().len())
        .max();

    for found_match in matches {
        println!(
            "{}",
            found_match.format_as_result(&pattern, max_path_length, max_line_number_length)
        );
    }

    Ok(())
}
