// Copyright 2018 The Grin Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Configuration file management

use dirs;
use rand::distributions::{Alphanumeric, Distribution};
use rand::thread_rng;
use std::env;
use std::fs::{self, File};
use std::io::prelude::*;
use std::io::BufReader;
use std::io::Read;
use std::path::PathBuf;
use toml;

use crate::comments::insert_comments;
use crate::core::global;
use crate::p2p;
use crate::servers::ServerConfig;
use crate::types::{ConfigError, ConfigMembers, GlobalConfig};
use crate::util::LoggingConfig;

/// The default file name to use when trying to derive
/// the node config file location
pub const SERVER_CONFIG_FILE_NAME: &'static str = "epic-server.toml";
const SERVER_LOG_FILE_NAME: &'static str = "epic-server.log";
const EPIC_HOME: &'static str = ".epic";
const EPIC_CHAIN_DIR: &'static str = "chain_data";
/// Node API secret
pub const API_SECRET_FILE_NAME: &'static str = ".api_secret";

fn get_epic_path(chain_type: &global::ChainTypes) -> Result<PathBuf, ConfigError> {
	// Check if epic dir exists
	let mut epic_path = match dirs::home_dir() {
		Some(p) => p,
		None => PathBuf::new(),
	};
	epic_path.push(EPIC_HOME);
	epic_path.push(chain_type.shortname());
	// Create if the default path doesn't exist
	if !epic_path.exists() {
		fs::create_dir_all(epic_path.clone())?;
	}
	Ok(epic_path)
}

fn get_foundation_path(chain_type: &global::ChainTypes) -> String {
	let foundation_name = match *chain_type {
		global::ChainTypes::Mainnet => "foundation.json",
		_ => "foundation_floonet.json",
	};

	if cfg!(windows) {
		format!(".\\{}", foundation_name).to_owned()
	} else {
		format!("/usr/share/epic/{}", foundation_name).to_owned()
	}
}

fn check_config_current_dir(path: &str) -> Option<PathBuf> {
	let p = env::current_dir();
	let mut c = p.ok()?;
	c.push(path);
	if c.exists() {
		return Some(c);
	}
	None
}

/// Create file with api secret
pub fn init_api_secret(api_secret_path: &PathBuf) -> Result<(), ConfigError> {
	let mut api_secret_file = File::create(api_secret_path)?;
	let api_secret: String = Alphanumeric
		.sample_iter(&mut thread_rng())
		.take(20)
		.collect();
	api_secret_file.write_all(api_secret.as_bytes())?;
	Ok(())
}

/// Check if file contains a secret and nothing else
pub fn check_api_secret(api_secret_path: &PathBuf) -> Result<(), ConfigError> {
	let api_secret_file = File::open(api_secret_path)?;
	let buf_reader = BufReader::new(api_secret_file);
	let mut lines_iter = buf_reader.lines();
	let first_line = lines_iter.next();
	if first_line.is_none() || first_line.unwrap().is_err() {
		fs::remove_file(api_secret_path)?;
		init_api_secret(api_secret_path)?;
	}
	Ok(())
}

/// Check that the api secret file exists and is valid
fn check_api_secret_file(chain_type: &global::ChainTypes) -> Result<(), ConfigError> {
	let epic_path = get_epic_path(chain_type)?;
	let mut api_secret_path = epic_path.clone();
	api_secret_path.push(API_SECRET_FILE_NAME);
	if !api_secret_path.exists() {
		init_api_secret(&api_secret_path)
	} else {
		check_api_secret(&api_secret_path)
	}
}

/// Handles setup and detection of paths for node
pub fn initial_setup_server(chain_type: &global::ChainTypes) -> Result<GlobalConfig, ConfigError> {
	check_api_secret_file(chain_type)?;
	// Use config file if current directory if it exists, .epic home otherwise
	if let Some(p) = check_config_current_dir(SERVER_CONFIG_FILE_NAME) {
		GlobalConfig::new(p.to_str().unwrap())
	} else {
		// Check if epic dir exists
		let epic_path = get_epic_path(chain_type)?;

		// Get path to default config file
		let mut config_path = epic_path.clone();
		config_path.push(SERVER_CONFIG_FILE_NAME);

		// Spit it out if it doesn't exist
		if !config_path.exists() {
			let mut default_config = GlobalConfig::for_chain(chain_type);
			// update paths relative to current dir
			default_config.update_paths(&epic_path, chain_type);
			default_config.write_to_file(config_path.to_str().unwrap())?;
		}

		GlobalConfig::new(config_path.to_str().unwrap())
	}
}

/// Returns the defaults, as strewn throughout the code
impl Default for ConfigMembers {
	fn default() -> ConfigMembers {
		ConfigMembers {
			server: ServerConfig::default(),
			logging: Some(LoggingConfig::default()),
		}
	}
}

impl Default for GlobalConfig {
	fn default() -> GlobalConfig {
		GlobalConfig {
			config_file_path: None,
			members: Some(ConfigMembers::default()),
		}
	}
}

impl GlobalConfig {
	/// Same as GlobalConfig::default() but further tweaks parameters to
	/// apply defaults for each chain type
	pub fn for_chain(chain_type: &global::ChainTypes) -> GlobalConfig {
		let mut defaults_conf = GlobalConfig::default();
		let defaults = &mut defaults_conf.members.as_mut().unwrap().server;
		defaults.chain_type = chain_type.clone();

		match *chain_type {
			global::ChainTypes::Mainnet => {
				defaults.p2p_config.seeding_type = p2p::Seeding::List;
			}
			global::ChainTypes::Floonet => {
				defaults.api_http_addr = "127.0.0.1:13413".to_owned();
				defaults.p2p_config.port = 13414;
				defaults
					.stratum_mining_config
					.as_mut()
					.unwrap()
					.stratum_server_addr = Some("127.0.0.1:13416".to_owned());
				defaults
					.stratum_mining_config
					.as_mut()
					.unwrap()
					.wallet_listener_url = "http://127.0.0.1:13415".to_owned();
			}
			global::ChainTypes::UserTesting => {
				defaults.api_http_addr = "127.0.0.1:23413".to_owned();
				defaults.p2p_config.port = 23414;
				defaults.p2p_config.seeding_type = p2p::Seeding::None;
				defaults
					.stratum_mining_config
					.as_mut()
					.unwrap()
					.stratum_server_addr = Some("127.0.0.1:23416".to_owned());
				defaults
					.stratum_mining_config
					.as_mut()
					.unwrap()
					.wallet_listener_url = "http://127.0.0.1:23415".to_owned();
			}
			global::ChainTypes::AutomatedTesting => {
				panic!("Can't run automated testing directly");
			}
		}
		defaults_conf
	}

	/// Requires the path to a config file
	pub fn new(file_path: &str) -> Result<GlobalConfig, ConfigError> {
		let mut return_value = GlobalConfig::default();
		return_value.config_file_path = Some(PathBuf::from(&file_path));

		// Config file path is given but not valid
		let config_file = return_value.config_file_path.clone().unwrap();
		if !config_file.exists() {
			return Err(ConfigError::FileNotFoundError(String::from(
				config_file.to_str().unwrap(),
			)));
		}

		// Try to parse the config file if it exists, explode if it does exist but
		// something's wrong with it
		return_value.read_config()
	}

	/// Read config
	fn read_config(mut self) -> Result<GlobalConfig, ConfigError> {
		let mut file = File::open(self.config_file_path.as_mut().unwrap())?;
		let mut contents = String::new();
		file.read_to_string(&mut contents)?;
		let decoded: Result<ConfigMembers, toml::de::Error> = toml::from_str(&contents);
		match decoded {
			Ok(gc) => {
				self.members = Some(gc);
				return Ok(self);
			}
			Err(e) => {
				return Err(ConfigError::ParseError(
					String::from(self.config_file_path.as_mut().unwrap().to_str().unwrap()),
					String::from(format!("{}", e)),
				));
			}
		}
	}

	/// Update paths
	pub fn update_paths(&mut self, epic_home: &PathBuf, chain_type: &global::ChainTypes) {
		// need to update server chain path
		let mut chain_path = epic_home.clone();
		chain_path.push(EPIC_CHAIN_DIR);
		self.members.as_mut().unwrap().server.db_root = chain_path.to_str().unwrap().to_owned();
		let mut secret_path = epic_home.clone();
		secret_path.push(API_SECRET_FILE_NAME);

		self.members.as_mut().unwrap().server.api_secret_path =
			Some(secret_path.to_str().unwrap().to_owned());

		self.members.as_mut().unwrap().server.foundation_path = get_foundation_path(chain_type);

		let mut log_path = epic_home.clone();
		log_path.push(SERVER_LOG_FILE_NAME);

		self.members
			.as_mut()
			.unwrap()
			.logging
			.as_mut()
			.unwrap()
			.log_file_path = log_path.to_str().unwrap().to_owned();
	}

	/// Enable mining
	pub fn stratum_enabled(&mut self) -> bool {
		return self
			.members
			.as_mut()
			.unwrap()
			.server
			.stratum_mining_config
			.as_mut()
			.unwrap()
			.enable_stratum_server
			.unwrap();
	}

	/// Serialize config
	pub fn ser_config(&mut self) -> Result<String, ConfigError> {
		let encoded: Result<String, toml::ser::Error> =
			toml::to_string(self.members.as_mut().unwrap());
		encoded.map_err(|e| ConfigError::SerializationError(String::from(format!("{}", e))))
	}

	/// Write configuration to a file
	pub fn write_to_file(&mut self, name: &str) -> Result<(), ConfigError> {
		let conf_out = self.ser_config()?;
		let conf_out = insert_comments(conf_out);
		let mut file = File::create(name)?;
		file.write_all(conf_out.as_bytes())?;
		Ok(())
	}
}
