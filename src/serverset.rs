extern crate log;
extern crate discotech_zookeeper;

use config::*;

use rustc_serialize::json;
use std::sync::RwLock;
use std::thread;
use std::time::Duration;
use std::collections::HashMap;
use std::collections::HashSet;
use discotech_zookeeper::{Acl, CreateMode, Watcher, WatchedEvent, ZkError, ZooKeeper};
use discotech_zookeeper::perms;


#[derive(Debug, RustcDecodable, RustcEncodable)]
pub struct ServiceEndpoint {
  pub host: String,
  pub port: u16,
}


#[derive(Debug, RustcDecodable, RustcEncodable)]
pub struct ServersetMember {
  pub serviceEndpoint: ServiceEndpoint,
  pub additionalEndpoints: HashMap<String, ServiceEndpoint>,
  pub status: String,
}


struct NullWatcher;
impl Watcher for NullWatcher {
  fn handle(&self, e: &WatchedEvent) {}
}


pub struct Serverset {
  config: DiscoConfig,
  zk_client: ZooKeeper,
  pub members: RwLock<HashMap<String, ServersetMember>>,
}
impl Serverset {
  pub fn new(discoConfig: DiscoConfig) -> Serverset {
    match ZooKeeper::connect(format!("{}:{}/",
        discoConfig.zookeeper_host, discoConfig.zookeeper_port).as_str(),
        Duration::from_secs(discoConfig.zookeeper_timeout_secs), NullWatcher) {
      Err(reason) => panic!("Unable to connect to ZooKeeper: {}", reason),
      Ok(client) => Serverset{
        config: discoConfig,
        zk_client: client,
        members: RwLock::new(HashMap::new()),
      },
    }
  }

  pub fn watch(&self) {
    let zk_client = self.zk_client.clone();
    thread::spawn(move || {
      self.update_members(zk_client);
      thread::sleep_ms(self.config.zookeeper_poll_ms);
    });
  }

  fn remove_member(&self, member_znode: &String) {
    self.members.write().unwrap().remove(member_znode);
  }

  fn znode_exists(&self, zk_client: ZooKeeper, znode: &String) -> bool {
    match zk_client.exists(znode, false) {
      Ok(_) => true,
      _ => false,
    }
  }

  fn update_member(&self, zk_client: ZooKeeper, member_znode: &String) {
    debug!("Adding Serverset member: {}", member_znode);
    // If the Serverset member's ZNode does not exist, does not update the member.
    let full_member_znode = format!("{}/{}", self.config.serverset_znode, member_znode);
    if !self.znode_exists(zk_client, &full_member_znode) {
      return
    }
    // Reads Serverset member's ZNode data and attempts to parse it into a String.
    let member_json_opt = match self.zk_client.get_data(full_member_znode.as_str(),
        false) {
      Err(reason) => {
        error!("Could not obtain node data for {} from ZooKeeper: {}", member_znode,
            reason);
        None
      },
      Ok(node_data) => match String::from_utf8(node_data.0) {
        Err(reason) => {
          error!("Could not parse node string: {}", reason);
          None
        },
        Ok(node_string) => Some(node_string),
      },
    };

    // Attempts to parse Serverset member's ZNode into a ServersetMember struct.
    let member_opt: Option<ServersetMember> = match member_json_opt {
      None => None,
      Some(member_json) => match json::decode(&member_json) {
        Err(reason) => {
          error!("Could not parse node JSON: {}", reason);
          None
        },
        Ok(member) => Some(member),
      },
    };

    // If all has gone well, grabs a write lock on the members HashMap and updates it with
    // the newly-unwrapped ServersetMember.
    match member_opt {
      None => None,
      Some(member) => match member.status.as_ref() {
        "ALIVE" => {
          self.members.write().unwrap().insert(member_znode.clone(), member)
        },
        _ => None,
      },
    };
  }

  fn update_members(&self, zk_client: ZooKeeper) {
    // Reconciles our local representation of the Serverset with that which has been
    // stored in ZooKeeper.
    debug!("Updating Serverset members...");

    if !self.znode_exists(zk_client, &self.config.serverset_znode) {
      error!("Could not find Serverset ZNode: {}", self.config.serverset_znode);
      return
    }
    match zk_client.get_children(self.config.serverset_znode.as_str(), false) {
      Err(reason) => error!("Unable to get children for {}: {}",
          self.config.serverset_znode, reason),
      Ok(serverset_children) => {
        debug!("Children: {:?}", serverset_children);
        // Updates all serverset members in parallel, tracking those which we've seen.
        let mut current_member_znodes = HashSet::new();
        for current_member_znode in serverset_children.iter() {
          current_member_znodes.insert(current_member_znode);
          self.update_member(zk_client, current_member_znode);
        }
        // Removes all members that have dropped out of the serverset.
        for old_member_znode in self.members.read().unwrap().keys() {
          if !current_member_znodes.contains(old_member_znode) {
            self.remove_member(old_member_znode);
          }
        }
      },
    }
  }
}
