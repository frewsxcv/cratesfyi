//! This is a simle crate module

use std::io::prelude::*;
use std::io::BufReader;
use std::io::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::collections;
use std::env;

use cargo;
use toml;
use rustc_serialize::json::{encode, Json, ParserError, EncoderError, ToJson};
use postgres;
use hyper::client::Client;
use time;
use regex::Regex;
use slug::slugify;

use super::{DocBuilder, DocBuilderError, copy_files, command_result};


/// Really simple crate model
#[derive(Debug)]
pub struct Crate {
    /// Name of crate
    pub name: String,
    /// Versions of crate
    pub versions: Vec<String>,
}


#[derive(Debug)]
pub enum CrateOpenError {
    FileNotFound,
    EncoderError(EncoderError),
    ParseError(ParserError),
    IoError(Error),
    ManifestError(Box<cargo::util::errors::CargoError>),
    NotObject,
    NameNotFound,
    VersNotFound,
    DbError(postgres::error::Error),
    CommandError(String),
    DocBuilderError(DocBuilderError),
}


#[derive(Debug)]
pub struct CrateInfo {
    pub name: String,
    pub target_name: String,
    pub version: String,
    pub dependencies: Vec<(String, String)>,
    pub rustdoc: Option<String>,
    pub readme: Option<String>,
    pub metadata: cargo::core::manifest::ManifestMetadata,
}



impl Crate {
    /// Returns a new Crate
    pub fn new(name: String, versions: Vec<String>) -> Crate {
        Crate {
            name: name,
            versions: versions,
        }
    }

    /// Creates a new crate from crates.io-index path file
    pub fn from_cargo_index_file(path: PathBuf) -> Result<Crate, CrateOpenError> {

        let reader = try!(fs::File::open(path).map(|f| BufReader::new(f)));

        let mut name = String::new();
        let mut versions = Vec::new();

        for line in reader.lines() {
            let line = try!(line);
            let (cname, vers) = try!(Crate::parse_cargo_index_line(&line));
            name = cname;
            versions.push(vers);
        }

        versions.reverse();

        Ok(Crate {
            name: name,
            versions: versions
        })
    }


    /// Create Crate from crate name and try to find it in crates.io-index
    /// to load versions
    pub fn from_cargo_index_path(name: &str, path: &PathBuf) -> Result<Crate, CrateOpenError> {

        if !path.is_dir() {
            return Err(CrateOpenError::FileNotFound);
        }

        for file in try!(path.read_dir()) {

            let file = try!(file);

            let path = file.path();

            // skip files under .git and config.json
            if path.to_str().unwrap().contains(".git") ||
                path.file_name().unwrap() == "config.json" {
                    continue;
                }

            if path.is_dir() {
                if let Ok(c) = Crate::from_cargo_index_path(&name, &path) {
                    return Ok(c);
                }
            } else if file.file_name().into_string().unwrap() == name {
                return Crate::from_cargo_index_file(path);
            }

        }

        Err(CrateOpenError::FileNotFound)
    }


    fn parse_cargo_index_line(line: &String) -> Result<(String, String), CrateOpenError> {
        let data = try!(Json::from_str(line.trim()).map_err(CrateOpenError::ParseError));
        let obj = try!(data.as_object().ok_or(CrateOpenError::NotObject));

        let crate_name = try!(obj.get("name")
                              .and_then(|n| n.as_string())
                              .ok_or(CrateOpenError::NameNotFound));

        let vers = try!(obj.get("vers")
                        .and_then(|n| n.as_string())
                        .ok_or(CrateOpenError::VersNotFound));

        Ok((String::from(crate_name), String::from(vers)))
    }


    /// Returns index of requested_version
    pub fn get_version_index(&self, requested_version: &str) -> Option<usize> {
        for i in 0..self.versions.len() {
            if self.versions[i] == requested_version {
                return Some(i);
            }
        }
        None
    }


    /// Returns index of version if it starts with it
    pub fn version_starts_with(&self, version: &str) -> Option<usize> {
        // if version is "*" return latest version index which is 0
        if version == "*" {
            return Some(0);
        }
        for i in 0..self.versions.len() {
            if self.versions[i].starts_with(&version) {
                return Some(i)
            }
        }
        None
    }


    /// Returns canonical name of crate, i.e: "rand-0.1.13"
    pub fn canonical_name(&self, version_index: usize) -> String {
        format!("{}-{}", self.name, self.versions[version_index])
    }


    /// Extracts crate into CWD
    pub fn extract_crate(&self, version_index: usize) -> Result<String, String> {
        let crate_name = format!("{}.crate", self.canonical_name(version_index));
        command_result(Command::new("tar")
                       .arg("-xzvf")
                       .arg(crate_name)
                       .output()
                       .unwrap())
    }


    /// Downloads crate into CWD
    pub fn download_crate(&self, version_index: usize) -> Result<String, String> {
        // By default crates.io is using:
        // https://crates.io/api/v1/crates/$crate/$version/download
        // But I believe this url is increasing download count and this program is
        // downloading alot during development. I am using redirected url.
        let url = format!("https://crates-io.s3-us-west-1.amazonaws.com/crates/{}/{}-{}.crate",
                          self.name,
                          self.name,
                          self.versions[version_index]);
        // Use wget for now
        command_result(Command::new("wget")
                       .arg("-c")
                       .arg("--content-disposition")
                       .arg(url)
                       .output()
                       .unwrap())
    }



    /// Download local dependencies from crate root and place them into right place
    ///
    /// Some packages have local dependencies defined in Cargo.toml
    ///
    /// This function is intentionall written verbose
    fn download_dependencies(&self, root_dir: &PathBuf, docbuilder: &DocBuilder) -> Result<(), DocBuilderError> {

        let mut cargo_toml_path = PathBuf::from(&root_dir);
        cargo_toml_path.push("Cargo.toml");

        let mut cargo_toml_fh = try!(fs::File::open(cargo_toml_path)
                                     .map_err(DocBuilderError::LocalDependencyIoError));
        let mut cargo_toml_content = String::new();
        try!(cargo_toml_fh.read_to_string(&mut cargo_toml_content)
             .map_err(DocBuilderError::LocalDependencyIoError));

        toml::Parser::new(&cargo_toml_content[..]).parse().as_ref()
            .and_then(|cargo_toml| cargo_toml.get("dependencies"))
            .and_then(|dependencies| dependencies.as_table())
            .and_then(|dependencies_table| self.get_local_dependencies(dependencies_table, docbuilder))
            .map(|local_dependencies| self.handle_local_dependencies(local_dependencies, &root_dir))
            .unwrap_or(Ok(()))
    }


    /// Get's local_dependencies from dependencies_table
    fn get_local_dependencies(&self,
                              dependencies_table: &collections::BTreeMap<String, toml::Value>,
                              docbuilder: &DocBuilder) -> Option<Vec<(Crate, usize, String)>>  {

        let mut local_dependencies: Vec<(Crate, usize, String)> = Vec::new();

        for key in dependencies_table.keys() {

            dependencies_table.get(key)
                .and_then(|key_val| key_val.as_table())
                .map(|key_table| {
                    key_table.get("path").and_then(|p| p.as_str()).map(|path| {
                        key_table.get("version").and_then(|p| p.as_str())
                            .map(|version| {
                                // TODO: This kinda became a mess
                                //       I wonder if can use more and_then's...
                                if let Ok(dep_crate) = Crate::from_cargo_index_path(&key,
                                                            &docbuilder.crates_io_index_path) {
                                    if let Some(version_index) =
                                        dep_crate.version_starts_with(version) {
                                        local_dependencies.push((dep_crate,
                                                                 version_index,
                                                                 path.to_string()));
                                    }
                                }
                            });
                    });
                });

        }
        Some(local_dependencies)
    }


    /// Handles local dependencies
    fn handle_local_dependencies(&self,
                                 local_dependencies: Vec<(Crate, usize, String)>,
                                 root_dir: &PathBuf) -> Result<(), DocBuilderError> {
        for local_dependency in local_dependencies {
            let crte = local_dependency.0;
            let version_index = local_dependency.1;

            let mut path = PathBuf::from(&root_dir);
            path.push(local_dependency.2);

            // make sure path exists
            if !path.exists() {
                try!(fs::create_dir_all(&path).map_err(DocBuilderError::LocalDependencyIoError));
            }

            try!(crte.download_crate(version_index)
                 .map_err(DocBuilderError::LocalDependencyDownloadError));
            try!(crte.extract_crate(version_index)
                 .map_err(DocBuilderError::LocalDependencyExtractCrateError));

            let crte_download_dir = PathBuf::from(format!("{}-{}",
                                                          crte.name,
                                                          crte.versions[version_index]));

            if !crte_download_dir.exists() {
                return Err(DocBuilderError::LocalDependencyDownloadDirNotExist);
            }


            // self.extract_crate will extract crate into build_dir
            // Copy files to proper location
            try!(copy_files(&crte_download_dir, &path));

            // Remove download_dir
            try!(fs::remove_dir_all(&crte_download_dir)
                 .map_err(DocBuilderError::LocalDependencyIoError));

            try!(crte.remove_crate_file(version_index));
        }

        Ok(())
    }


    fn remove_build_dir_for_crate(&self,
                                  version_index: usize) -> Result<(), DocBuilderError> {
        let path = PathBuf::from(self.canonical_name(version_index));

        if path.exists() && path.is_dir() {
            try!(fs::remove_dir_all(&path).map_err(DocBuilderError::RemoveBuildDir));
        }

        Ok(())
    }


    /// Builds crate documentation
    pub fn build_crate_doc(&self,
                           version_index: usize,
                           docbuilder: &DocBuilder) -> Result<(), DocBuilderError> {


        let package_root = PathBuf::from(self.canonical_name(version_index));

        info!("Building documentation for {}-{}", self.name, self.versions[version_index]);

        // removing old build directory
        try!(self.remove_build_dir_for_crate(version_index));

        // Download crate
        // FIXME: Need to capture failed command outputs
        info!("Downloading crate\n{}",
              try!(self.download_crate(version_index)
                   .map_err(DocBuilderError::DownloadCrateError)));

        // Extract crate
        info!("Extracting crate\n{}",
              try!(self.extract_crate(version_index)
                   .map_err(DocBuilderError::ExtractCrateError)));

        info!("Checking local dependencies");
        try!(self.download_dependencies(&package_root, &docbuilder));

        // build docs
        info!("Building documentation");
        let (status, message) = match self.build_doc(version_index) {
            Ok(m) => (true, m),
            Err(m) => (false, m),
        };
        info!("cargo doc --no-deps --verbose\n{}", message);

        if status {
            Ok(())
        } else {
            Err(DocBuilderError::FailedToBuildCrate)
        }
    }


    fn build_doc(&self, version_index: usize) -> Result<String, String> {
        let cwd = env::current_dir().unwrap();
        let mut target = PathBuf::from(&cwd);
        target.push(self.canonical_name(version_index));
        env::set_current_dir(target).unwrap();
        let res = command_result(Command::new("cargo")
                                 .arg("doc")
                                 .arg("--no-deps")
                                 .arg("--verbose")
                                 .output()
                                 .unwrap());
        env::set_current_dir(cwd).unwrap();
        res
    }


    /// Removes crate file if it's exists in CWD
    pub fn remove_crate_file(&self,
                             version_index: usize) -> Result<(), DocBuilderError>{
        let path = PathBuf::from(format!("{}.crate", self.canonical_name(version_index)));

        if path.exists() && path.is_file() {
            try!(fs::remove_file(path).map_err(DocBuilderError::RemoveCrateFile));
        }

        Ok(())
    }


    /// Get manifest of a crate. This function assumes crate downloaded and exracted.
    pub fn manifest(&self,
                    version_index: usize)
    -> Result<cargo::core::manifest::Manifest, CrateOpenError> {
        let cwd = env::current_dir().unwrap();
        let mut package_root = PathBuf::from(&cwd);
        package_root.push(self.canonical_name(version_index));
        let (manifest, _) = try!(path_to_manifest(package_root.as_path()).
                                 map_err(CrateOpenError::ManifestError));

        Ok(manifest)
    }


    /// Gets CrateInfo. This function assumes crate downloaded and exracted.
    pub fn info(&self, version_index: usize) -> Result<CrateInfo, CrateOpenError> {
        let cwd = env::current_dir().unwrap();
        let mut package_root = PathBuf::from(&cwd);
        package_root.push(self.canonical_name(version_index));
        info_from_path(package_root.as_path())
    }


    /// Adds crate into database
    pub fn add_crate_into_database(&self,
                                   version_index: usize,
                                   conn: &postgres::Connection,
                                   docbuilder: &DocBuilder) -> Result<(), CrateOpenError> {

        let crate_id: i32 = {
            let mut rows = try!(conn.query("SELECT id FROM crates WHERE name = $1",
                                           &[&self.name]));
            // insert crate into database if it is not exists
            if rows.len() == 0 {
                rows = try!(conn.query("INSERT INTO crates (name) VALUES ($1) RETURNING id",
                                       &[&self.name]));
            }
            rows.get(0).get(0)
        };

        let (crate_info, have_examples) = {

            fn have_examples(path: &PathBuf) -> bool {
                let path = PathBuf::from(path).join("examples");
                path.exists() && path.is_dir()
            }

            // check source directory
            let mut path = PathBuf::from(&docbuilder.sources_path);
            path.push(&self.name);
            path.push(&self.versions[version_index]);
            if path.exists() {
                (try!(info_from_path(&path)), have_examples(&path))
            } else {
                try!(self.download_crate(version_index).map_err(CrateOpenError::CommandError));
                try!(self.extract_crate(version_index).map_err(CrateOpenError::CommandError));
                let mut path = PathBuf::from(env::current_dir().unwrap());
                path.push(self.canonical_name(version_index));
                let info = try!(info_from_path(&path));
                try!(self.remove_crate_file(version_index)
                     .map_err(CrateOpenError::DocBuilderError));
                try!(self.remove_build_dir_for_crate(version_index)
                     .map_err(CrateOpenError::DocBuilderError));
                (info, have_examples(&path))
            }
        };


        let dependencies = try!(encode(&crate_info.dependencies)
                                .map_err(CrateOpenError::EncoderError));

        let (release_time, yanked, downloads) = {
            let url = format!("https://crates.io/api/v1/crates/{}/versions", self.name);
            // FIXME: There is probably better way to do this
            //        and so many unwraps...
            let client = Client::new();
            let mut res = client.get(&url[..]).send().unwrap();
            let mut body = String::new();
            res.read_to_string(&mut body).unwrap();
            let json = Json::from_str(&body[..]).unwrap();
            let versions = try!(json.as_object()
                .and_then(|o| o.get("versions"))
                .and_then(|v| v.as_array())
                .ok_or(CrateOpenError::NotObject));

            let (mut release_time, mut yanked, mut downloads) = (None, None, None);

            for version in versions {
                let version = try!(version.as_object().ok_or(CrateOpenError::NotObject));
                let version_num = try!(version.get("num").and_then(|v| v.as_string())
                    .ok_or(CrateOpenError::NotObject));

                if version_num == self.versions[version_index] {
                    let release_time_raw = try!(version.get("created_at")
                                                .and_then(|c| c.as_string())
                                                .ok_or(CrateOpenError::NotObject));
                    release_time = Some(time::strptime(release_time_raw,
                                                       "%Y-%m-%dT%H:%M:%S").unwrap()
                                        .to_timespec());

                    yanked = Some(try!(version.get("yanked").and_then(|c| c.as_boolean())
                        .ok_or(CrateOpenError::NotObject)));

                    downloads = Some(try!(version.get("downloads").and_then(|c| c.as_i64())
                        .ok_or(CrateOpenError::NotObject)) as i32);

                    break;
                }
            }

            (release_time, yanked, downloads)
        };


        let (build_status, rustdoc_status) = {
            let mut build_log_path = PathBuf::from(&docbuilder.logs_path);
            build_log_path.push(format!("{}/{}-{}.log",
                                        self.name,
                                        self.name,
                                        self.versions[version_index]));

            let mut crate_doc_path = PathBuf::from(&docbuilder.destination);
            crate_doc_path.push(&self.name);
            crate_doc_path.push(&self.versions[version_index]);

            // Build is _most likely successfully_ if build log
            // and crate doc directory exists
            let build_status = if build_log_path.exists() && crate_doc_path.exists() {
                1
            }
            // Build is most likely failed if crate build log is exists
            // but documentation directory isn't
            else if build_log_path.exists() && !crate_doc_path.exists() {
                -1
            }
            // cratesfyi not tried to build this crate if build log is not exists
            else {
                0
            };


            // check existence of first target in manifest
            // in destination directory to find out rustdoc status
            crate_doc_path.push(crate_info.target_name);
            let rustdoc_status = if crate_doc_path.exists() { 1 } else { 0 };

            (build_status, rustdoc_status)
        };


        // TODO: Add test status
        let test_status = 0;

        let release_id: i32 = {
            let rows = try!(conn.query("SELECT id FROM releases \
                                       WHERE crate_id = $1 AND version = $2",
                                       &[&crate_id, &crate_info.version]));

            // Add release into database if it's not exists
            if rows.len() == 0 {
                let rows = try!(conn.query("INSERT INTO releases ( \
                                               crate_id,         version,        release_time, \
                                               dependencies,     yanked,         build_status, \
                                               rustdoc_status,   test_status,    license, \
                                               repository_url,   homepage_url,   description, \
                                               description_long, readme,         authors, \
                                               keywords,         have_examples,  downloads \
                                           ) \
                                           VALUES ( \
                                               $1,  $2,  $3,  $4,  $5,  $6,  $7, $8, $9, $10, \
                                               $11, $12, $13, $14, $15, $16, $17, $18 \
                                           ) RETURNING id",
                                           &[
                                               &crate_id,
                                               &crate_info.version,
                                               &release_time,
                                               &Json::from_str(&dependencies[..]).unwrap(),
                                               &yanked,
                                               &build_status,
                                               &rustdoc_status,
                                               &test_status,
                                               &crate_info.metadata.license,
                                               &crate_info.metadata.repository,
                                               &crate_info.metadata.homepage,
                                               &crate_info.metadata.description,
                                               &crate_info.rustdoc,
                                               &crate_info.readme,
                                               &Json::from_str(
                                                   &encode(&crate_info.metadata.authors).unwrap())
                                                   .unwrap(),
                                               &Json::from_str(
                                                   &encode(&crate_info.metadata.keywords).unwrap())
                                                   .unwrap(),
                                               &have_examples,
                                               &downloads,
                                           ]));
                // return id
                rows.get(0).get(0)
            } else {
                try!(conn.query("UPDATE releases \
                                 SET release_time = $3, \
                                     dependencies = $4,      yanked = $5, \
                                     build_status = $6,      rustdoc_status = $7, \
                                     test_status = $8,       license = $9, \
                                     repository_url = $10,   homepage_url = $11, \
                                     description = $12,      description_long = $13, \
                                     readme = $14,           authors = $15, \
                                     keywords = $16,         have_examples = $17, \
                                     downloads = $18 \
                                 WHERE crate_id = $1 AND version = $2",
                                 &[
                                     &crate_id,
                                     &crate_info.version,
                                     &release_time,
                                     &Json::from_str(&dependencies[..]).unwrap(),
                                     &yanked,
                                     &build_status,
                                     &rustdoc_status,
                                     &test_status,
                                     &crate_info.metadata.license,
                                     &crate_info.metadata.repository,
                                     &crate_info.metadata.homepage,
                                     &crate_info.metadata.description,
                                     &crate_info.rustdoc,
                                     &crate_info.readme,
                                     &Json::from_str(
                                         &encode(&crate_info.metadata.authors).unwrap())
                                         .unwrap(),
                                     &Json::from_str(
                                         &encode(&crate_info.metadata.keywords).unwrap())
                                         .unwrap(),
                                     &have_examples,
                                     &downloads,
                                 ]));
                rows.get(0).get(0)
            }
        };



        // Add keywords into database
        for keyword in crate_info.metadata.keywords {
            let slug = slugify(&keyword);
            let keyword_id: i32 = {
                let rows = try!(conn.query("SELECT id FROM keywords WHERE slug = $1",
                                           &[&slug]));
                if rows.len() > 0 {
                    rows.get(0).get(0)
                } else {
                    try!(conn.query("INSERT INTO keywords (name, slug) \
                                    VALUES ($1, $2) RETURNING id",
                                    &[&keyword, &slug])).get(0).get(0)
                }
            };
            // add releationship
            let _ = conn.query("INSERT INTO keyword_rels (rid, kid) VALUES ($1, $2)",
                               &[&release_id, &keyword_id]);
        }

        // Add authors into database
        let author_capture_re = Regex::new("^([^><]+)<*(.*?)>*$").unwrap();
        for author in crate_info.metadata.authors {
            if let Some(author_captures) = author_capture_re.captures(&author[..]) {
                let author = author_captures.at(1).unwrap_or("").trim();
                let email = author_captures.at(2).unwrap_or("").trim();
                let slug = slugify(&author);

                let author_id: i32 = {
                    let rows = try!(conn.query("SELECT id FROM authors WHERE slug = $1",
                                               &[&slug]));
                    if rows.len() > 0 {
                        rows.get(0).get(0)
                    } else {
                        try!(conn.query("INSERT INTO authors (name, email, slug) \
                                        VALUES ($1, $2, $3) RETURNING id",
                                        &[&author, &email, &slug])).get(0).get(0)
                    }
                };

                // add relationship
                let _ = conn.query("INSERT INTO author_rels (rid, aid) VALUES ($1, $2)",
                                   &[&release_id, &author_id]);
            }
        }


        // Add owners into database
        // owners available in: https://crates.io/api/v1/crates/rand/owners
        {
            let owners_url = format!("https://crates.io/api/v1/crates/{}/owners", self.name);
            let client = Client::new();
            let mut res = client.get(&owners_url[..]).send().unwrap();
            // FIXME: There is probably better way to do this
            //        and so many unwraps...
            let mut body = String::new();
            res.read_to_string(&mut body).unwrap();
            let json = try!(Json::from_str(&body[..]).map_err(CrateOpenError::ParseError));

            if let Some(owners) = json.as_object().and_then(|j| j.get("users"))
                                                  .and_then(|j| j.as_array()) {
                for owner in owners {
                    let avatar = owner.as_object().and_then(|o| o.get("avatar"))
                                      .and_then(|o| o.as_string()).unwrap_or("");
                    let email = owner.as_object().and_then(|o| o.get("email"))
                                     .and_then(|o| o.as_string()).unwrap_or("");
                    let login = owner.as_object().and_then(|o| o.get("login"))
                                     .and_then(|o| o.as_string()).unwrap_or("");
                    let name = owner.as_object().and_then(|o| o.get("name"))
                                    .and_then(|o| o.as_string()).unwrap_or("");
                    let slug = slugify(&name);

                    if login.is_empty() {
                        continue;
                    }

                    let owner_id: i32 = {
                        let rows = try!(conn.query("SELECT id FROM owners WHERE login = $1",
                                                   &[&login]));
                        if rows.len() > 0 {
                            rows.get(0).get(0)
                        } else {
                            try!(conn.query("INSERT INTO owners (login, slug, avatar, name, email) \
                                            VALUES ($1, $2, $3, $4, $5) RETURNING id",
                                            &[&login, &slug, &avatar, &name, &email]))
                                .get(0).get(0)
                        }
                    };

                    // add relationship
                    let _ = conn.query("INSERT INTO owner_rels (cid, oid) VALUES ($1, $2)",
                                       &[&crate_id, &owner_id]);
                }

            }
        }


        // Update versions
        {
            let mut versions: Json = try!(conn.query("SELECT versions FROM crates \
                                                     WHERE id = $1",
                                                     &[&crate_id])).get(0).get(0);
            if let Some(versions_array) = versions.as_array_mut() {
                let mut found = false;
                for version in versions_array.clone() {
                    if version.as_string().unwrap() == self.versions[version_index] {
                        found = true;
                    }
                }

                if !found {
                    versions_array.push(self.versions[version_index].to_json());
                }
            }

            let _ = conn.query("UPDATE crates SET versions = $1 WHERE id = $2",
                               &[&versions, &crate_id]);
        }

        Ok(())
    }

}



/// Generates cargo::core::manifest::Manifest from a crate path
pub fn path_to_manifest(root_dir: &Path) ->
cargo::util::errors::CargoResult<(cargo::core::manifest::Manifest, Vec<PathBuf>)> {
    let cargo_config = try!(cargo::util::config::Config::default());
    let source_id = try!(cargo::core::source::SourceId::for_path(&root_dir));

    // read Cargo.toml
    let mut cargo_toml_path = PathBuf::from(&root_dir);
    cargo_toml_path.push("Cargo.toml");

    let mut cargo_toml_fh = try!(fs::File::open(cargo_toml_path));
    let mut cargo_toml_content = Vec::new();
    try!(cargo_toml_fh.read_to_end(&mut cargo_toml_content));

    let layout = cargo::util::toml::project_layout(root_dir);

    cargo::util::toml::to_manifest(&cargo_toml_content[..], &source_id, layout, &cargo_config)
}



/// Gets crate info from path
pub fn info_from_path(path: &Path) -> Result<CrateInfo, CrateOpenError> {
    debug!("Getting info from path: {:?}", path);
    let (manifest, _) = try!(path_to_manifest(path).
                             map_err(CrateOpenError::ManifestError));

    let rustdoc = if manifest.targets()[0].src_path().is_absolute() {
        try!(read_rust_doc(manifest.targets()[0].src_path()))
    } else {
        let mut path = PathBuf::from(&path);
        path.push(manifest.targets()[0].src_path());
        try!(read_rust_doc(path.as_path()))
    };

    let readme = {
        if manifest.metadata().readme.is_some() {
            let mut readme_path = PathBuf::from(path);
            readme_path.push(manifest.metadata().readme.clone().unwrap());

            let mut reader = try!(fs::File::open(readme_path).map(|f| BufReader::new(f)));
            let mut readme = String::new();
            reader.read_to_string(&mut readme).unwrap();
            Some(readme)
        } else {
            None
        }
    };

    let mut dependencies: Vec<(String, String)> = Vec::new();

    for dependency in manifest.dependencies() {
        let name = dependency.name().to_string();
        let version = format!("{}", dependency.version_req());
        dependencies.push((name, version));
    }

    Ok(CrateInfo {
        name: manifest.name().to_string(),
        target_name: manifest.targets()[0].name().to_string(),
        version: format!("{}", manifest.summary().version()),
        dependencies: dependencies,
        rustdoc: rustdoc,
        readme: readme,
        metadata: manifest.metadata().clone()
    })
}


/// Gets rustdoc from file
fn read_rust_doc(file_path: &Path) -> Result<Option<String>, CrateOpenError> {
    debug!("Reading rustdoc from: {:?}", file_path);
    let reader = try!(fs::File::open(file_path).map(|f| BufReader::new(f)));
    let mut rustdoc = String::new();

    for line in reader.lines() {
        let line = try!(line);
        if line.starts_with("//!") {
            if line.len() > 3 {
                rustdoc.push_str(line.split_at(4).1);
            }
            rustdoc.push('\n');
        }
    }

    if rustdoc.is_empty() {
        Ok(None)
    } else {
        Ok(Some(rustdoc))
    }

}



impl From<postgres::error::Error> for CrateOpenError {
    fn from(err: postgres::error::Error) -> CrateOpenError {
        CrateOpenError::DbError(err)
    }
}

impl From<Error> for CrateOpenError {
    fn from(err: Error) -> CrateOpenError {
        CrateOpenError::IoError(err)
    }
}



#[cfg(test)]
mod test {
    extern crate env_logger;
    use super::*;
    use std::env;
    use std::path::PathBuf;

    #[test]
    fn test_get_vesion_index() {
        let crte = Crate::new("cratesfyi".to_string(),
                              vec!["0.1.0".to_string(), "0.1.1".to_string()]);
        assert_eq!(crte.get_version_index("0.1.0"), Some(0));
        assert_eq!(crte.get_version_index("0.1.1"), Some(1));
        assert_eq!(crte.get_version_index("0.1.2"), None);
    }


    // Rest of the tests only works if crates.io-index is exists in:
    // ../cratesfyi-prefix/crates.io-index

    #[test]
    #[ignore]
    fn test_from_cargo_index_path() {
        let mut path = PathBuf::from(env::current_dir().unwrap());
        path.push("../cratesfyi-prefix/crates.io-index");

        if !path.exists() {
            return;
        }

        let crte = Crate::from_cargo_index_path("rand", &path).unwrap();
        assert_eq!(crte.name, "rand");
        assert!(crte.versions.len() > 0);
    }


    #[test]
    #[ignore]
    fn test_version_starts_with() {
        let mut path = PathBuf::from(env::current_dir().unwrap());
        path.push("../cratesfyi-prefix/crates.io-index");

        if !path.exists() {
            return;
        }

        let crte = Crate::from_cargo_index_path("rand", &path).unwrap();
        assert!(crte.version_starts_with("0.1").is_some());
        assert!(crte.version_starts_with("*").is_some());
        assert!(crte.version_starts_with("999.099.99").is_none());
    }


    #[test]
    fn test_download_extract_remove_crate() {
        let crte = Crate::new("rand".to_string(),
                              vec!["0.3.13".to_string()]);
        assert!(crte.download_crate(0).is_ok());
        assert!(crte.extract_crate(0).is_ok());

        let path = PathBuf::from(crte.canonical_name(0));
        assert!(path.exists());

        assert!(crte.remove_crate_file(0).is_ok());
        assert!(crte.remove_build_dir_for_crate(0).is_ok());
    }


    #[test]
    fn test_path_to_manifest() {
        let _ = env_logger::init();
        let crte = Crate::new("calculator".to_string(), vec!["0.0.1".to_string()]);

        assert!(crte.download_crate(0).is_ok());
        assert!(crte.extract_crate(0).is_ok());

        let cwd = env::current_dir().unwrap();
        let mut package_root = PathBuf::from(&cwd);
        package_root.push(crte.canonical_name(0));

        let res = path_to_manifest(package_root.as_path());

        info!("MANIFEST:\n{:#?}", res);
        assert!(res.is_ok());

        // remove downloaded stuff
        assert!(crte.remove_crate_file(0).is_ok());
        assert!(crte.remove_build_dir_for_crate(0).is_ok());
    }



    #[test]
    fn test_crate_info() {
        let _ = env_logger::init();
        let crte = Crate::new("rand".to_string(), vec!["0.3.9".to_string()]);

        crte.download_crate(0).unwrap();
        crte.extract_crate(0).unwrap();
        let info = crte.info(0);

        info!("CRATE INFO: {:#?}", info);

        assert!(info.is_ok());

        let info = info.unwrap();

        assert!(info.rustdoc.is_some());
        assert!(!info.rustdoc.unwrap().is_empty());

        assert!(info.readme.is_some());
        assert!(!info.readme.unwrap().is_empty());

        // remove downloaded stuff
        assert!(crte.remove_crate_file(0).is_ok());
        assert!(crte.remove_build_dir_for_crate(0).is_ok());
    }


    #[test]
    #[ignore]
    fn test_add_crate_into_database() {
        use ::db;
        use docbuilder::DocBuilder;
        let _ = env_logger::init();
        let conn = db::connect_db().unwrap();
        let docbuilder = DocBuilder::default();
        let crte = Crate::new("rand".to_string(), vec!["0.3.14".to_string()]);
        let res = crte.add_crate_into_database(0, &conn, &docbuilder);

        info!("Result: {:?}", res);

        assert!(res.is_ok());
    }
}
