use crdts::CmRDT;

use error::Result;
use map;
use data::{Data, Op, Actor, Kind};
use log::{TaggedOp, LogReplicable};

pub type Map = map::Map<(Vec<u8>, Kind), Data, Actor>;

pub struct DB<L: LogReplicable<Actor, Map>> {
    log: L,
    remote_logs: Vec<L>,
    map: Map
}

impl<L: LogReplicable<Actor, Map>> DB<L> {
    pub fn new(log: L, map: Map) -> Self {
        DB { log, remote_logs: Vec::new(), map }
    }

    pub fn get(&self, key: &(Vec<u8>, Kind)) -> Result<Option<Data>> {
        self.map.get(key)
    }

    pub fn update<F>(&mut self, key: (Vec<u8>, Kind), actor: Actor, updater: F) -> Result<()>
        where F: FnOnce(Data) -> Option<Op>
    {
        let map_op = self.map.update(key, actor, updater)?;
        let tagged_op = self.log.commit(map_op)?;
        self.map.apply(tagged_op.op())?;
        self.log.ack(&tagged_op)
    }

    pub fn rm(&mut self, key: (Vec<u8>, Kind), actor: Actor) -> Result<()> {
        let op = self.map.rm(key, actor)?;
        let tagged_op = self.log.commit(op)?;
        self.map.apply(tagged_op.op())?;
        self.log.ack(&tagged_op)
    }

    pub fn sync(&mut self) -> Result<()> {
        for mut remote_log in self.remote_logs.iter_mut() {
            self.log.pull(&remote_log)?;
            self.log.push(&mut remote_log)?;
        }

        while let Some(tagged_op) = self.log.next()? {
            self.map.apply(tagged_op.op())?;
            self.log.ack(&tagged_op)?;
        }
        Ok(())
    }
}
