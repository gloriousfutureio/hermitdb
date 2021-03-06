extern crate gitdb;
extern crate tempfile;

#[macro_use]
extern crate assert_matches;

use gitdb::data::{Prim, Op, Kind, Actor};
use gitdb::{memory_log, map, sled, db, DB};

fn mk_db(actor: Actor) -> DB<memory_log::Log<Actor, db::Map>> {
    let config = sled::ConfigBuilder::new().temporary(true).build();
    let tree = sled::Tree::start(config).unwrap();
    let log = memory_log::Log::new(actor);
    let map = map::Map::new(tree);
    DB::new(log, map)
}

#[test]
fn test_write_read_set() {
    let mut db = mk_db(1);

    assert_matches!(db.get(&("x".as_bytes().to_vec(), Kind::Set)), Ok(None));

    assert_matches!(
        db.update(("x".as_bytes().to_vec(), Kind::Set), 1, |data| {
            let mut set = data.set().unwrap();
            Some(Op::Set(set.add(Prim::Float(57.18), 1)))
        }),
        Ok(())
    );

    assert_eq!(
        db.get(&("x".as_bytes().to_vec(), Kind::Set)).unwrap().unwrap().set().unwrap().value(),
        vec![Prim::Float(57.18)]
    );
}
