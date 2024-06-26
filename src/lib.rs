pub mod download_utils;
pub mod versions;
pub mod profile_manager;
pub mod options;
pub mod process;
pub mod progress_reporter;
#[cfg(test)]
mod tests;

use std::{
  path::{ PathBuf, MAIN_SEPARATOR_STR },
  fs::{ self, create_dir_all, File },
  env::consts::ARCH,
  collections::{ HashMap, HashSet },
  ops::Deref,
  io::{ self, Write },
  sync::Arc,
};

use chrono::{ Utc, Timelike };
use download_utils::{ ProxyOptions, download_job::DownloadJob };
use log::{ info, error, debug, warn };
use options::{ GameOptions, MinecraftFeatureMatcher };
use os_info::Type::Windows;
use process::GameProcess;
use progress_reporter::ProgressReporter;
use regex::Regex;
use serde_json::json;
use thiserror::Error;
use versions::{
  VersionManager,
  json::{ rule::{ FeatureMatcher, RuleFeatureType, OperatingSystem }, LocalVersionInfo, AssetIndex },
  info::VersionInfo,
};
use zip::ZipArchive;

use crate::{ versions::json::{ ArgumentType, library::ExtractRules, Sha1Sum }, process::GameProcessBuilder };

#[derive(Error, Debug)]
#[error("{0}")]
pub struct MinecraftLauncherError(String);

const DEFAULT_JRE_ARGUMENTS_32BIT: &str =
  "-Xmx2G -XX:+UnlockExperimentalVMOptions -XX:+UseG1GC -XX:G1NewSizePercent=20 -XX:G1ReservePercent=20 -XX:MaxGCPauseMillis=50 -XX:G1HeapRegionSize=32M";
const DEFAULT_JRE_ARGUMENTS_64BIT: &str =
  "-Xmx2G -XX:+UnlockExperimentalVMOptions -XX:+UseG1GC -XX:G1NewSizePercent=20 -XX:G1ReservePercent=20 -XX:MaxGCPauseMillis=50 -XX:G1HeapRegionSize=32M";

pub struct MinecraftGameRunner {
  options: GameOptions,
  feature_matcher: Box<MinecraftFeatureMatcher>,
  version_manager: VersionManager,
  local_version: Option<LocalVersionInfo>,

  natives_dir: Option<PathBuf>,
  virtual_dir: Option<PathBuf>,
}

impl MinecraftGameRunner {
  pub fn new(options: GameOptions) -> Self {
    let feature_matcher = Box::new(MinecraftFeatureMatcher(false, options.resolution.clone()));
    let version_manager = VersionManager::new(options.game_dir.clone(), feature_matcher.clone());

    Self {
      options,
      feature_matcher,
      version_manager,

      local_version: None,
      natives_dir: None,
      virtual_dir: None,
    }
  }

  fn get_local_version(&self) -> &LocalVersionInfo {
    self.local_version.as_ref().unwrap()
  }

  fn get_virtual_dir(&self) -> &PathBuf {
    &self.virtual_dir.as_ref().unwrap()
  }

  fn get_natives_dir(&self) -> &PathBuf {
    &self.natives_dir.as_ref().unwrap()
  }

  fn get_version_dir(&self) -> PathBuf {
    self.options.game_dir.join("versions").join(&self.options.version.to_string())
  }

  fn get_assets_dir(&self) -> PathBuf {
    self.options.game_dir.join("assets")
  }

  fn get_asset_index(&self) -> Option<AssetIndex> {
    let asset_index_id = &self.get_local_version().asset_index.as_ref()?.id;
    let asset_index_json_path = self.get_assets_dir().join("indexes").join(format!("{}.json", asset_index_id));

    let file = &mut File::open(asset_index_json_path).ok()?;
    Some(serde_json::from_reader(file).ok()?)
  }

  fn is_win_ten(&self) -> bool {
    let os = os_info::get();
    os.os_type() == Windows && os.edition().is_some_and(|edition| edition.contains("Windows 10"))
  }

  fn progress_reporter(&self) -> &Arc<ProgressReporter> {
    &self.options.progress_reporter
  }
}

impl MinecraftGameRunner {
  pub async fn launch(&mut self) -> Result<GameProcess, Box<dyn std::error::Error>> {
    // TODO: maybe initialize everything here and avoid initializing another instance with the same game runner until it's completed
    self.progress_reporter().set("Fetching version manifest", 0, 2);
    self.version_manager.refresh().await?;
    info!("Queuing library & version downloads");

    self.progress_reporter().set_status("Resolving local version").set_progress(1);
    let mut local_version = match self.version_manager.get_local_version(&self.options.version) {
      Some(local_version) => local_version,
      None => { self.version_manager.install_version(&self.options.version).await? }
    };

    if !local_version.applies_to_current_environment(self.feature_matcher.deref()) {
      return Err(
        MinecraftLauncherError(format!("Version {} is is incompatible with the current environment", self.options.version.to_string())).into()
      );
    }

    if !self.version_manager.is_up_to_date(&local_version).await {
      local_version = self.version_manager.install_version(&self.options.version).await?;
    }

    local_version = local_version.resolve(&self.version_manager, HashSet::new()).await?;

    self.progress_reporter().clear();
    // TODO: self.migrate_old_assets()
    self.download_required_files(&local_version).await?;

    self.local_version = Some(local_version);
    self.launch_game().await
  }

  async fn download_required_files(&self, local_version: &LocalVersionInfo) -> Result<(), Box<dyn std::error::Error>> {
    let mut job1 = DownloadJob::new(
      "Version & Libraries",
      false,
      self.options.max_concurrent_downloads,
      self.options.max_download_attempts,
      self.progress_reporter()
    );
    self.version_manager.download_version(&self, local_version, &mut job1)?;

    let mut job2 = DownloadJob::new(
      "Resources",
      false,
      self.options.max_concurrent_downloads,
      self.options.max_download_attempts,
      self.progress_reporter()
    );
    job2.add_downloadables(self.version_manager.get_resource_files(&self.options.proxy, &self.options.game_dir, &local_version).await.unwrap());

    job1.start().await?;
    job2.start().await?;
    Ok(())
  }

  async fn launch_game(&mut self) -> Result<GameProcess, Box<dyn std::error::Error>> {
    info!("Launching game");

    let natives_dir = self.get_version_dir().join(format!("{}-natives-{}", self.options.version.to_string(), Utc::now().nanosecond()));
    if !natives_dir.is_dir() {
      fs::create_dir_all(&natives_dir)?;
    }

    info!("Unpacking natives to {}", natives_dir.display());

    if let Err(err) = self.unpack_natives(&natives_dir) {
      error!("Couldn't unpack natives! {err}");
      Err(MinecraftLauncherError(format!("Couldn't unpack natives! {err}")))?;
    }

    let virtual_dir = self.reconstruct_assets();
    if let Err(err) = &virtual_dir {
      error!("Couldn't unpack natives! {err}");
      Err(MinecraftLauncherError(format!("Couldn't unpack natives! {err}")))?;
    }
    self.virtual_dir = virtual_dir.ok();

    self.natives_dir = Some(natives_dir);

    let game_dir = &self.options.game_dir;
    info!("Launching in {}", game_dir.display());
    if !game_dir.exists() {
      if let Err(_) = fs::create_dir_all(&game_dir) {
        error!("Aborting launch; couldn't create game directory");
        Err(MinecraftLauncherError("Aborting launch; couldn't create game directory".to_string()))?;
      }
    } else if !game_dir.is_dir() {
      error!("Aborting launch; game directory is not actually a directory");
      Err(MinecraftLauncherError("Aborting launch; game directory is not actually a directory".to_string()))?;
    }

    let server_resource_packs_dir = game_dir.join("server-resource-packs");
    create_dir_all(&server_resource_packs_dir)?;

    let mut game_process_builder = GameProcessBuilder::new();
    game_process_builder.with_java_path(&self.options.java_path);
    game_process_builder.directory(game_dir);

    if let Some(jvm_args) = &self.options.jvm_args {
      game_process_builder.with_arguments(jvm_args.clone());
    } else {
      let args = if ARCH == "x86_64" { DEFAULT_JRE_ARGUMENTS_64BIT } else { DEFAULT_JRE_ARGUMENTS_32BIT };
      game_process_builder.with_arguments(
        args
          .split(" ")
          .map(|s| s.to_string())
          .collect()
      );
    }

    let substitutor = self.create_arguments_substitutor();

    // Add JVM args
    let local_version = self.local_version.as_ref().unwrap();
    if !local_version.arguments.is_empty() {
      if let Some(arguments) = local_version.arguments.get(&ArgumentType::Jvm) {
        game_process_builder.with_arguments(
          arguments
            .iter()
            .map(|v| v.apply(self.feature_matcher.deref()))
            .flatten()
            .flatten()
            .cloned()
            .map(&substitutor)
            .collect::<Vec<_>>()
        );
      }
    } else if let Some(_) = &local_version.minecraft_arguments {
      if OperatingSystem::get_current_platform() == OperatingSystem::Windows {
        game_process_builder.with_argument("-XX:HeapDumpPath=MojangTricksIntelDriversForPerformance_javaw.exe_minecraft.exe.heapdump");
        if self.is_win_ten() {
          game_process_builder.with_arguments(vec!["-Dos.name=Windows 10", "-Dos.version=10.0"]);
        }
      } else if OperatingSystem::get_current_platform() == OperatingSystem::Osx {
        game_process_builder.with_arguments(vec![&substitutor("-Xdock:icon=${asset=icons/minecraft.icns}".to_string()), "-Xdock:name=Minecraft"]);
      }

      game_process_builder.with_argument(&substitutor("-Djava.library.path=${natives_directory}".to_string()));
      game_process_builder.with_argument(&substitutor("-Dminecraft.launcher.brand=${launcher_name}".to_string()));
      game_process_builder.with_argument(&substitutor("-Dminecraft.launcher.version=${launcher_version}".to_string()));
      game_process_builder.with_argument(&substitutor("-Dminecraft.client.jar=${primary_jar}".to_string()));
      game_process_builder.with_arguments(vec!["-cp".to_string(), substitutor("${classpath}".to_string())]);
    }

    game_process_builder.with_argument(&local_version.get_main_class());
    info!("Half command: {}", game_process_builder.get_args().join(" "));
    if !local_version.arguments.is_empty() {
      if let Some(arguments) = local_version.arguments.get(&ArgumentType::Game) {
        game_process_builder.with_arguments(
          arguments
            .iter()
            .map(|v| v.apply(self.feature_matcher.deref()))
            .flatten()
            .flatten()
            .cloned()
            .map(&substitutor)
            .collect::<Vec<_>>()
        );
      }
    } else if let Some(minecraft_arguments) = &local_version.minecraft_arguments {
      game_process_builder.with_arguments(
        minecraft_arguments
          .split(" ")
          .map(|s| s.to_string())
          .map(&substitutor)
          .collect::<Vec<_>>()
      );

      if self.feature_matcher.has_feature(&RuleFeatureType::IsDemoUser, &json!(true)) {
        game_process_builder.with_argument("--demo");
      }

      if self.feature_matcher.has_feature(&RuleFeatureType::HasCustomResolution, &json!(true)) {
        game_process_builder.with_arguments(
          vec![
            "--width".to_string(),
            substitutor("${resolution_width}".to_string()),
            "--height".to_string(),
            substitutor("${resolution_height}".to_string())
          ]
        );
      }
    }

    // TODO: get proxy auth?
    if let ProxyOptions::Proxy(url) = &self.options.proxy {
      game_process_builder.with_arguments(vec!["--proxyHost".to_string(), url.host_str().unwrap().to_string()]);
      game_process_builder.with_arguments(vec!["--proxyPort".to_string(), url.port().unwrap().to_string()]);

      if !url.username().is_empty() {
        game_process_builder.with_arguments(vec!["--proxyUser".to_string(), url.username().to_string()]);
      }

      if let Some(passowrd) = url.password() {
        game_process_builder.with_arguments(vec!["--proxyPass".to_string(), passowrd.to_string()]);
      }
    }

    {
      // Remove token from args
      let mut args = game_process_builder.get_args().join(" ");
      let token = self.options.authentication.get_authenticated_token();
      if !token.is_empty() {
        args = args.replace(&token, "?????");
      }
      debug!("Running {} {}", &self.options.java_path.display(), args);
    }

    let regex = Regex::new(r"\$\{.+\}")?;
    game_process_builder
      .get_args()
      .iter()
      .filter_map(|arg| regex.find(arg))
      .for_each(|arg| debug!("Unresolved variable - {:?}", arg.as_str()));

    let process = game_process_builder.spawn();

    self.perform_cleanups()?;

    match process {
      Ok(process) => Ok(process),
      Err(err) => Err(Box::new(MinecraftLauncherError(format!("Failed to launch game: {err}")))),
    }
  }

  fn perform_cleanups(&self) -> Result<(), Box<dyn std::error::Error>> {
    // this.cleanupOrphanedVersions();
    // this.cleanupOrphanedAssets();
    // this.cleanupOldSkins();
    self.cleanup_old_natives()?;
    // this.cleanupOldVirtuals();
    Ok(())
  }

  fn cleanup_old_natives(&self) -> Result<(), Box<dyn std::error::Error>> {
    let game_dir = &self.version_manager.game_dir;

    let current_time = Utc::now().timestamp_millis() as u128;
    // let time_threshold = Duration::from_secs(3600);

    for local_ver in self.version_manager.get_local_versions() {
      let version_id = local_ver.get_id().to_string();
      let version_dir = game_dir.join("versions").join(&version_id);
      let dirs: Vec<PathBuf> = fs
        ::read_dir(&version_dir)?
        .filter_map(|file| file.ok())
        .filter(|file| file.file_type().unwrap().is_dir())
        .map(|file| file.file_name().to_str().unwrap().to_string())
        .filter(|name| name.starts_with(&format!("{version_id}-natives-")))
        .map(|name| version_dir.join(name))
        .collect();
      for native_dir in dirs {
        let modified_time = native_dir.metadata()?.modified()?;
        if current_time - modified_time.elapsed()?.as_millis() >= 3600000 {
          debug!("Deleting {}", native_dir.display());
          if let Err(err) = fs::remove_dir_all(&native_dir) {
            warn!("Failed to delete {}: {}", native_dir.display(), err);
          }
        }
      }
    }
    Ok(())
  }

  fn unpack_natives(&self, natives_dir: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let os = OperatingSystem::get_current_platform();
    let libs = self.local_version.as_ref().unwrap().get_relevant_libraries(self.feature_matcher.deref());

    fn unpack_native(
      natives_dir: &PathBuf,
      mut zip_archive: ZipArchive<File>,
      extract_rules: Option<&ExtractRules>
    ) -> Result<(), Box<dyn std::error::Error>> {
      for i in 0..zip_archive.len() {
        let mut file = zip_archive.by_index(i).unwrap();
        let file_zip_path = file.enclosed_name().unwrap().to_owned();
        if let Some(extract_rules) = extract_rules {
          if !extract_rules.should_extract(&file_zip_path) {
            continue;
          }
        }

        let output_file = natives_dir.join(file_zip_path);
        create_dir_all(output_file.parent().unwrap())?;
        if file.is_dir() {
          continue;
        }

        let mut output_file = File::create(output_file)?;
        io::copy(&mut file, &mut output_file)?;
      }
      Ok(())
    }

    for lib in libs {
      let natives = &lib.natives;
      if let Some(native_id) = natives.get(&os) {
        let file = &self.options.game_dir.join("libraries").join(lib.get_artifact_path(Some(native_id)).replace("/", MAIN_SEPARATOR_STR));

        let zip_file = ZipArchive::new(File::open(file)?)?;
        let extract_rules = lib.extract.as_ref();
        let _ = unpack_native(natives_dir, zip_file, extract_rules); // Ignore errors
      }
    }

    Ok(())
  }

  fn reconstruct_assets(&self) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let assets_dir = self.options.game_dir.join("assets"); //self.assets_dir;
    let indexes_dir = assets_dir.join("indexes");
    let objects_dir = assets_dir.join("objects");
    let asset_index_id = &self.get_local_version().asset_index.as_ref().unwrap().id;
    let asset_index_file = indexes_dir.join(format!("{}.json", asset_index_id));
    let mut virtual_dir = assets_dir.join("virtual").join(asset_index_id);

    if !asset_index_file.is_file() {
      warn!("No assets index file {}; can't reconstruct assets", virtual_dir.display());
      return Ok(virtual_dir);
    } else {
      let asset_index: AssetIndex = serde_json::from_reader(File::open(asset_index_file)?)?;
      if asset_index.map_to_resources {
        virtual_dir = self.options.game_dir.join("resources");
      }

      if asset_index.is_virtual || asset_index.map_to_resources {
        info!("Reconstructing virtual assets folder at {}", virtual_dir.display());

        for asset_obj_entry in asset_index.get_file_map() {
          let asset_file = virtual_dir.join(asset_obj_entry.0);
          let object_file = objects_dir.join(&asset_obj_entry.1.hash.to_string()[0..2]).join(asset_obj_entry.1.hash.to_string());

          let mut should_copy = true;
          if asset_file.is_file() {
            let hash = Sha1Sum::from_reader(&mut File::open(&asset_file)?)?;
            if hash != asset_obj_entry.1.hash {
              should_copy = true;
            }
          }

          if should_copy {
            info!("Copying asset for virtual or resource-mapped: {}", asset_file.display());
            fs::copy(object_file, asset_file)?;
          }
        }

        let mut last_used_file = File::create(virtual_dir.join(".lastused"))?;
        last_used_file.write_all(&Utc::now().to_rfc3339().as_bytes())?;
      }
    }

    Ok(virtual_dir)
  }

  fn create_arguments_substitutor(&self) -> impl Fn(String) -> String {
    let mut substitutor = ArgumentSubstitutorBuilder::new();

    let classpath_separator = if OperatingSystem::get_current_platform() == OperatingSystem::Windows { ";" } else { ":" };
    let version_id = self.options.version.to_string();
    let local_version = self.get_local_version();
    let game_dir = &self.options.game_dir;

    let classpath = self.construct_classpath(self.local_version.as_ref().unwrap()).unwrap();
    let assets_dir = self.get_assets_dir();
    let libraries_dir = game_dir.join("libraries");
    let natives_dir = self.get_natives_dir();
    let virtual_dir = self.get_virtual_dir();

    let launcher_opts = self.options.launcher_options.as_ref();

    let jar_id = local_version.get_jar().to_string();
    let jar_path = game_dir.join("versions").join(&jar_id).join(format!("{}.jar", &jar_id));

    let asset_index_substitutions = {
      let mut map = HashMap::new();

      if let Some(asset_index) = self.get_asset_index() {
        for (asset_name, asset) in asset_index.get_file_map() {
          let hash = asset.hash.to_string();
          let asset_path = assets_dir
            .join("objects")
            .join(&hash[0..2])
            .join(hash)
            .to_str()
            .unwrap()
            .to_string();
          map.insert(format!("asset={asset_name}"), asset_path);
        }
      }

      map
    };

    substitutor
      .add("auth_access_token", self.options.authentication.get_authenticated_token())
      .add("auth_session", self.options.authentication.get_auth_session())

      .add("auth_player_name", self.options.authentication.auth_player_name())
      .add("auth_uuid", self.options.authentication.auth_uuid().to_string())
      .add("user_type", self.options.authentication.user_type());

    substitutor
      .add("profile_name", "")
      .add("version_name", &version_id)
      .add("game_directory", game_dir.to_str().unwrap())
      .add("game_assets", virtual_dir.to_str().unwrap())
      .add("assets_root", assets_dir.to_str().unwrap())
      .add("assets_index_name", &local_version.asset_index.as_ref().unwrap().id)
      .add("version_type", &local_version.get_type().get_name());

    if let Some(resolution) = self.options.resolution.as_ref() {
      substitutor.add("resolution_width", &resolution.width().to_string());
      substitutor.add("resolution_height", &resolution.height().to_string());
    } else {
      substitutor.add("resolution_width", "");
      substitutor.add("resolution_height", "");
    }

    substitutor.add("language", "en-us").add_all(asset_index_substitutions);

    if let Some(launcher_opts) = launcher_opts {
      substitutor.add("launcher_name", &launcher_opts.launcher_name).add("launcher_version", &launcher_opts.launcher_version);
    } else {
      substitutor.add("launcher_name", "").add("launcher_version", "");
    }

    substitutor
      .add("natives_directory", natives_dir.to_str().unwrap())

      .add("classpath", &classpath)
      .add("classpath_separator", classpath_separator)
      .add("primary_jar", jar_path.to_str().unwrap());

    substitutor
      .add("clientid", "") // TODO: figure out
      .add("auth_xuid", ""); // TODO: only for msa

    substitutor.add("library_directory", &libraries_dir.to_str().unwrap()); // Forge compatibility

    substitutor.add_all(self.options.authentication.get_extra_substitutors());
    substitutor.add_all(self.options.substitutor_overrides.clone()); // Override if needed

    substitutor.build()
  }

  fn construct_classpath(&self, local_version: &LocalVersionInfo) -> Result<String, MinecraftLauncherError> {
    let os = OperatingSystem::get_current_platform();
    let separator = if os == OperatingSystem::Windows { ";" } else { ":" };
    let classpath = local_version.get_classpath(&os, &self.options.game_dir, self.feature_matcher.deref());
    for path in &classpath {
      if !path.is_file() {
        return Err(MinecraftLauncherError(format!("Classpath file not found: {}", path.display())));
      }
    }
    Ok(
      classpath
        .iter()
        .map(|s| s.to_str().unwrap().to_string())
        .collect::<Vec<_>>()
        .join(separator)
    )
  }
}

pub struct ArgumentSubstitutorBuilder {
  map: HashMap<String, String>,
}

impl ArgumentSubstitutorBuilder {
  pub fn new() -> Self {
    Self { map: HashMap::new() }
  }

  pub fn add(&mut self, key: impl AsRef<str>, value: impl AsRef<str>) -> &mut Self {
    self.map.insert(key.as_ref().to_string(), value.as_ref().to_string());
    self
  }

  pub fn add_all(&mut self, map: HashMap<impl AsRef<str>, impl AsRef<str>>) -> &mut Self {
    for (key, value) in map {
      self.add(key, value);
    }
    self
  }

  pub fn build(self) -> impl Fn(String) -> String {
    move |input| {
      let mut output = input;
      for (key, value) in &self.map {
        output = output.replace(&format!("${{{}}}", key.to_string()), &value);
      }
      output
    }
  }
}
