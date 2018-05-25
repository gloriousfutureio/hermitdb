// TODO: rename KEY_FILE to ENTROPY_FILE
extern crate time;
extern crate git2;
extern crate rmp_serde;
extern crate ditto;
extern crate ring;

use self::git2::Repository;

use std;

use db_error::{Result, DBErr};
use crypto::{Session, Plaintext, Encrypted, Config, gen_rand_256};
use remote::Remote;
use block::{Prim, Block, Blockable};
use encoding;

pub struct DB {
    pub root: std::path::PathBuf,
    pub repo: git2::Repository
}

impl DB {
    pub fn init(root: &std::path::Path, mut sess: &mut Session) -> Result<DB> {
        println!("initializing gitdb at {:?}", root);
        let repo = Repository::open(&root)
            .or_else(|_| Repository::init(&root))
            .map_err(DBErr::Git)?;

        let db = DB {
            root: root.to_path_buf(),
            repo: repo
        };

        db.create_key_salt(&mut sess)?;
        db.consistancy_check(&mut sess)?;
        Ok(db)
    }

    pub fn init_from_remote(root: &std::path::Path, remote: &Remote, mut sess: &mut Session) -> Result<DB> {
        println!("initializing from remote");
        let repo = Repository::init(&root)
            .map_err(DBErr::Git)?;

//        {
//            // make an initial commit
//            println!("making initial commit");
//            let mut index = repo.index().map_err(DBErr::Git)?;
//            let tree = index.write_tree()
//                .and_then(|tree_oid| repo.find_tree(tree_oid))
//                .map_err(DBErr::Git)?;
//
//            println!("pre-commit");
//            // commit the current tree
//            let sig = repo.signature()
//                .map_err(DBErr::Git)?;
//
//            let commit_msg = format!("initial commit from site: {}", sess.site_id);
//            repo.commit(Some("HEAD"), &sig, &sig, &commit_msg, &tree, &[])
//                .map_err(DBErr::Git)?;
//        
//            println!("finished initial commit");
//        }
        {
            let mut git_remote = repo.remote(&remote.name(), &remote.url())
                .map_err(DBErr::Git)?;

            println!("fetched remote");

            let mut fetch_opt = git2::FetchOptions::new();
            fetch_opt.remote_callbacks(remote.git_callbacks());
            git_remote.fetch(&["master"], Some(&mut fetch_opt), None)
                .map_err(DBErr::Git)?;
        }
        println!("looking for remote master branch..");
        if let Ok(branch) = repo.find_branch(&format!("{}/master", &remote.name()), git2::BranchType::Remote) {
            println!("found remote master branch!");
            let remote_branch_commit_oid = branch
                .get()
                .resolve()
                .map_err(DBErr::Git)
                ?.target()
                .ok_or(DBErr::State("remote ref didn't resolve to commit".into()))?;

            let remote_commit = repo
                .find_commit(remote_branch_commit_oid)
                .map_err(DBErr::Git)?;

            repo.branch("master", &remote_commit, false)
                .map_err(DBErr::Git)?;

            repo.set_head("refs/heads/master")
                .map_err(DBErr::Git)?;

            repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
                .map_err(DBErr::Git)?;
        } else {
            println!("remote master branch not found! initializing as a new repo");
            // remote is empty! initialize as normal
            let db = DB::init(&root, &mut sess)?;
            db.write_remote(&remote, &mut sess)?;
        };

        DB::init(&root, &mut sess)
    }

    fn consistancy_check(&self, mut sess: &mut Session) -> Result<()> {
        let cryptic = self.root.join("cryptic");
        if !cryptic.is_dir() {
            std::fs::create_dir(&cryptic).map_err(DBErr::IO)?;
        }
        Ok(())
    }

    fn create_key_salt(&self, mut sess: &mut Session) -> Result<()> {
        println!("creating key salt");
        let key_salt = std::path::Path::new("key_salt");
        let key_salt_filepath = self.root.join(&key_salt);

        if !key_salt_filepath.exists() {
            println!("key salt file {:?} not found so creating new one", key_salt_filepath);
            let salt = gen_rand_256()?;

            Plaintext {
                data: salt.to_vec(),
                config: Config::fresh_default()?
            }.encrypt(&mut sess)?.write(&key_salt_filepath)?;

            self.stage_file(&key_salt)?;
        }

        Ok(())
    }

    fn key_salt(&self, mut sess: &mut Session) -> Result<Vec<u8>> {
        println!("fetching key_salt");
        let key_salt_file = self.root.join("key_salt");
        let key_salt = Encrypted::read(&key_salt_file)
            ?.decrypt(&mut sess)
            ?.data;

        Ok(key_salt)
    }

    fn derive_key_filepath(&self, key: &str, mut sess: &mut Session) -> Result<std::path::PathBuf> {
        let key_salt = self.key_salt(&mut sess)?;
        let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
        ctx.update(&key_salt);
        // TAI: consider avoiding building the path string here
        //      we should be able to update the ctx with path components
        ctx.update(key.as_bytes());
        let digest = ctx.finish();
        let encoded_hash = encoding::encode(&digest.as_ref());
        let (dir_part, file_part) = encoded_hash.split_at(2);
        let filepath = std::path::PathBuf::from(dir_part)
            .join(file_part);

        Ok(filepath)
    }

    pub fn read_block(&self, key: &str, mut sess: &mut Session) -> Result<Block> {

        let block_filepath = self.root
            .join("cryptic")
            .join(&self.derive_key_filepath(&key, &mut sess)?);

        println!("read_block {}\n\t{:?}", key, block_filepath);
        if block_filepath.exists() {
            let plaintext = Encrypted::read(&block_filepath)?.decrypt(&mut sess)?;
            let block_reg: ditto::Register<Block> = rmp_serde::from_slice(&plaintext.data)
                .map_err(DBErr::SerdeDe)?;
            Ok(block_reg.get().to_owned())
        } else {
            Err(DBErr::NotFound)
        }
    }

    pub fn write(&self, prefix: &str, data: &impl Blockable, mut sess: &mut Session) -> Result<()> {
        for (suffix, block) in data.blocks().into_iter() {
            let mut key = String::with_capacity(prefix.len() + suffix.len());
            key.push_str(&prefix);
            key.push_str(&suffix);

            if key.len() == 0 {
                return Err(DBErr::State("Attempting to write empty key to root path".into()));
            }

            let rel_path = std::path::Path::new("cryptic")
                .join(self.derive_key_filepath(&key, &mut sess)?);
            let block_filepath = self.root
                .join(&rel_path);

            println!("write {}\n\t{:?}", key, rel_path);

            let register = if block_filepath.exists() {
                let plaintext = Encrypted::read(&block_filepath)?.decrypt(&mut sess)?;

                let mut existing_reg: ditto::Register<Block> = rmp_serde::from_slice(&plaintext.data)
                    .map_err(DBErr::SerdeDe)?;

                let mut existing_block = existing_reg.clone().get().to_owned();

                let new_block = match existing_block.merge(&block) {
                    Ok(()) => Ok(existing_block.to_owned()),
                    Err(DBErr::BlockTypeConflict) => Ok(block),
                    Err(e) => Err(e)
                }?;

                existing_reg.update(new_block, sess.site_id);
                existing_reg
            } else {
                ditto::Register::new(block, sess.site_id)
            };
        
            Plaintext {
                data: rmp_serde::to_vec(&register).map_err(DBErr::SerdeEn)?,
                config: Config::fresh_default()?
            }.encrypt(&mut sess)
                ?.write(&block_filepath)?;
            self.stage_file(&rel_path)?;
        }
        Ok(())
    }

    fn sync(&self, mut sess: &mut Session) -> Result<()> {
        // we assume all files to be synced have already been added to the index
        let mut index = self.repo.index().map_err(DBErr::Git)?;
        let tree = index.write_tree()
            .and_then(|tree_oid| self.repo.find_tree(tree_oid))
            .map_err(DBErr::Git)?;

        println!("pre-commit");
        // commit the current tree
        {
            let parent: Option<git2::Commit> = match self.repo.head() {
                Ok(head_ref) => {
                    let head_oid = head_ref.target()
                        .ok_or(DBErr::State(format!("Failed to find oid referenced by HEAD")))?;
                    let head_commit = self.repo.find_commit(head_oid)
                        .map_err(DBErr::Git)?;
                    Some(head_commit)
                },
                Err(_) => None // initial commit (no parent)
            };

            let sig = self.repo.signature()
                .map_err(DBErr::Git)?;

            let mut parent_commits = Vec::new();
            if let Some(ref commit) = parent {
                parent_commits.push(commit)
            }
            let commit_msg = format!("sync commit from site: {}", sess.site_id);
            self.repo.commit(Some("HEAD"), &sig, &sig, &commit_msg, &tree, &parent_commits)
                .map_err(DBErr::Git)?;
        }
        
        println!("post-commit");

        // fetch and merge
        let remote = self.read_remote(&mut sess)?;
        let mut git_remote = match self.repo.find_remote(&remote.name()) {
            Ok(git_remote) => git_remote,
            Err(_) => {
                // does not exist, we add this remote to git
                self.repo.remote(&remote.name(), &remote.url())
                    .map_err(DBErr::Git)?
            }
        };

        println!("fetched remote");

        let mut fetch_opt = git2::FetchOptions::new();
        fetch_opt.remote_callbacks(remote.git_callbacks());
        git_remote.fetch(&["master"], Some(&mut fetch_opt), None)
            .map_err(DBErr::Git)?;
        
        println!("entering find_branch if");
        if let Ok(branch) = self.repo.find_branch(&format!("{}/master", &remote.name()), git2::BranchType::Remote) {
            println!("in find_branch if");
            let remote_branch_commit_oid = branch
                .get()
                .resolve()
                .map_err(DBErr::Git)
                ?.target()
                .ok_or(DBErr::State("remote ref didn't resolve to commit".into()))?;

            let remote_annotated_commit = self.repo
                .find_annotated_commit(remote_branch_commit_oid)
                .map_err(DBErr::Git)?;

            let (analysis, _) = self.repo.merge_analysis(&[&remote_annotated_commit])
                .map_err(DBErr::Git)?;
            
            if analysis != git2::MergeAnalysis::ANALYSIS_UP_TO_DATE {
                let remote_commit = self.repo
                    .find_commit(remote_branch_commit_oid)
                    .map_err(DBErr::Git)?;
                
                // we handle the merge ourselves
                let remote_tree = remote_commit
                    .tree()
                    .map_err(DBErr::Git)?;

                // now the tricky part, detecting and handling conflicts
                // we want to merge the local tree with the remote_tree
                let diff = self.repo.diff_tree_to_tree(
                    Some(&tree),
                    Some(&remote_tree),
                    None // TODO: see if there are any diff options we can use
                ).map_err(DBErr::Git)?;

                println!("iterating foreach");
                diff.foreach(
                    &mut |delta, sim| {
                        println!(
                            "delta! {:?} {:?} {:?} {}",
                            delta.status(),
                            delta.old_file().path(),
                            delta.new_file().path(),
                            sim
                        );

                        match delta.status() {
                            git2::Delta::Modified => {
                                println!("both files modified");
                                let old = delta.old_file();
                                let new = delta.new_file();
                                self.merge_mod_files(&old, &new, &mut sess).is_ok()
                            },
                            git2::Delta::Added => {
                                println!("remote added a file");
                                self.merge_add_file(&delta.new_file()).is_ok()
                            },
                            git2::Delta::Deleted => {
                                // local additions are seen as deletions from the other tree
                                true
                            }
                            _ => unimplemented!()
                        }
                    },
                    None,
                    None,
                    None
                ).map_err(DBErr::Git)?;

                {
                    println!("merge commit");
                    let mut index = self.repo.index().map_err(DBErr::Git)?;
                    let tree = index.write_tree()
                        .and_then(|tree_oid| self.repo.find_tree(tree_oid))
                        .map_err(DBErr::Git)?;

                    // commit the current tree
                    {
                        let sig = self.repo.signature()
                            .map_err(DBErr::Git)?;

                        let head_oid = self.repo.head().map_err(DBErr::Git)?.target()
                            .ok_or(DBErr::State(format!("Failed to find oid referenced by HEAD")))?;
                        let head_commit = self.repo.find_commit(head_oid)
                            .map_err(DBErr::Git)?;

                        let commit_msg = format!("merge commit from site: {}", sess.site_id);
                        let parent_commits = &[&head_commit, &remote_commit];
                        self.repo.commit(Some("HEAD"), &sig, &sig, &commit_msg, &tree, parent_commits)
                            .map_err(DBErr::Git)?;
                    }
                    println!("done merge commit");
                }

            }
        }
        
        println!("pushing git_remote");

        let mut push_opt = git2::PushOptions::new();
        push_opt.remote_callbacks(remote.git_callbacks());

        git_remote.push(&[&"refs/heads/master"], Some(&mut push_opt))
            .map_err(DBErr::Git)?;
        println!("Finish push");
        
        // TAI: should return stats struct
        Ok(())
    }

    fn merge_add_file(&self, new: &git2::DiffFile) -> Result<()> {
        let rel_path = new.path()
            .ok_or_else(|| DBErr::State("added file doesn't have a path!?".into()))?;
        let filepath = self.root.join(&rel_path);

        println!("merging added file {:?}", rel_path);

        let new_blob = self.repo.find_blob(new.id()).map_err(DBErr::Git)?;
        Encrypted::from_bytes(&new_blob.content())?.write(&filepath)?;
        
        println!("wrote added file to workdir");
        self.stage_file(&rel_path)?;
        Ok(())
    }

    fn merge_mod_files(&self, old: &git2::DiffFile, new: &git2::DiffFile, mut sess: &mut Session) -> Result<()> {
        let rel_path = old.path()
            .ok_or_else(|| DBErr::State("old file doesn't have a path!?".into()))?;
        let filepath = self.root.join(&rel_path);

        println!("merging {:?}", rel_path);
        let old_blob = self.repo.find_blob(old.id()).map_err(DBErr::Git)?;
        let new_blob = self.repo.find_blob(new.id()).map_err(DBErr::Git)?;
        let old_cryptic = old_blob.content();
        let new_cryptic = new_blob.content();

        let old_plain = Encrypted::from_bytes(&old_cryptic)?.decrypt(&mut sess)?;
        let new_plain = Encrypted::from_bytes(&new_cryptic)?.decrypt(&mut sess)?;

        println!("decrypted old and new");
        
        let mut old_reg: ditto::Register<Block> = rmp_serde::from_slice(&old_plain.data)
            .map_err(DBErr::SerdeDe)?;

        let mut new_reg: ditto::Register<Block> = rmp_serde::from_slice(&new_plain.data)
            .map_err(DBErr::SerdeDe)?;

        println!("parsed old and new registers");

        let mut old_block = old_reg.clone().get().to_owned();
        let mut new_block = new_reg.clone().get().to_owned();

        let merged_block = match old_block.merge(&new_block) {
            Ok(()) => Ok(old_block.to_owned()),
            Err(DBErr::BlockTypeConflict) => Ok(new_block),
            Err(e) => Err(e)
        }?;

        old_reg.merge(&new_reg);
        old_reg.update(merged_block, sess.site_id);
        
        Plaintext {
            data: rmp_serde::to_vec(&old_reg).map_err(DBErr::SerdeEn)?,
            config: Config::fresh_default()?
        }.encrypt(&mut sess)?.write(&filepath)?;

        self.stage_file(&rel_path)?;

        Ok(())
    }

    pub fn read_remote(&self, mut sess: &mut Session) -> Result<Remote> {
        Remote::from_db("db$config$remote", &self, &mut sess)
    }

    pub fn write_remote(&self, remote: &Remote, mut sess: &mut Session) -> Result<()> {
        // TODO: remove the other remote before writing, read has a noauth bias
        // TODO: https://docs.rs/git2/0.7.0/git2/struct.Remote.html#method.is_valid_name
        self.write("db$config$remote", remote, &mut sess)
    }

    fn stage_file(&self, file: &std::path::Path) -> Result<()> {
        let mut index = self.repo.index().map_err(DBErr::Git)?;
        index.add_path(&file).map_err(DBErr::Git)?;
        index.write().map_err(DBErr::Git)?;
        Ok(())
    }
    
//    fn pull_remote(&self, remote: &Remote) -> Result<()> {
//        println!("Pulling from remote: {}", remote.name);
//        let mut git_remote = self.repo.find_remote(&remote.name)
//            .map_err(|e| format!("Failed to find remote {}: {:?}", remote.name, e))?;
//
//        let mut fetch_opt = git2::FetchOptions::new();
//        fetch_opt.remote_callbacks(remote.git_callbacks());
//        git_remote.fetch(&["master"], Some(&mut fetch_opt), None)
//            .map_err(|e| format!("Failed to fetch remote {}: {:?}", remote.name, e))?;
//
//        let branch_res = self.repo.find_branch("master", git2::BranchType::Remote);
//
//        if branch_res.is_err() {
//            return Ok(()); // remote does not have a tracking branch, this happens on initialization (client has not pushed yet)
//        }
//        
//        let remote_branch_oid = branch_res.unwrap().get() // branch reference
//            .resolve() // direct reference
//            .map_err(|e| format!("Failed to resolve remote branch {} OID: {:?}", remote.name, e))
//            ?.target() // OID of latest commit on remote branch
//            .ok_or(format!("Failed to fetch remote oid: remote {}", remote.name))?;
//
//        let remote_commit = self.repo
//            .find_annotated_commit(remote_branch_oid)
//            .map_err(|e| format!("Failed to find commit for remote banch {}: {:?}", remote.name, e))?;
//
//        self.repo.merge(&[&remote_commit], None, None)
//            .map_err(|e| format!("Failed merge from remote {}: {:?}", remote.name, e))?;
//        
//        let index = self.repo.index()
//            .map_err(|e| format!("Failed to read index: {:?}", e))?;
//
//        if index.has_conflicts() {
//            panic!("I don't know how to handle conflicts yet!!!!!!!!!!!!!");
//        }
//
//        let stats = self.repo.diff_index_to_workdir(None, None)
//            .map_err(|e| format!("Failed diff index: {:?}", e))?.stats()
//            .map_err(|e| format!("Failed to get diff stats: {:?}", e))?;
//
//        if stats.files_changed() > 0 {
//            println!("{} files changed (+{}, -{})",
//                     stats.files_changed(),
//                     stats.insertions(),
//                     stats.deletions());
//
//            let remote_commit = self.repo.find_commit(remote_branch_oid)
//                .map_err(|e| format!("Failed to find remote commit: {:?}", e))?;
//
//            let msg = format!("Mona Sync from {}: {}",
//                              remote.name,
//                              time::now().asctime());
//
//            self.commit(&msg, &vec![&remote_commit])?;
//            self.push_remote(&remote)?;
//        }
//        
//        // TAI: should return stats struct
//        Ok(())
//    }

//    pub fn push_remote(&self, remote: &Remote) -> Result<()> {
//        println!("Pushing to remote {} {}", remote.name, remote.url);
//        let mut git_remote = self.repo.find_remote(&remote.name)
//            .map_err(|e| format!("Failed to find remote with name {}: {:?}", remote.name, e))?;
//
//        let mut fetch_opt = git2::PushOptions::new();
//        fetch_opt.remote_callbacks(remote.git_callbacks());
//
//        git_remote.push(&[&"refs/heads/master:refs/heads/master"], Some(&mut fetch_opt))
//            .map_err(|e| format!("Failed to push remote {}: {:?}", remote.name, e))?;
//        println!("Finish push");
//        Ok(())
//    }

//    pub fn sync(&self, mut sess: &mut Session) -> Result<()> {
//        for remote in self.remotes(&mut sess)?.remotes.iter() {
//            self.pull_remote(&remote)?;
//        }
//
//        let mut index = self.repo.index()
//            .map_err(|e| format!("Failed to fetch index: {:?}", e))?;
//
//        let stats = self.repo.diff_index_to_workdir(None, None)
//            .map_err(|e| format!("Failed diff index: {:?}", e))?.stats()
//            .map_err(|e| format!("Failed to get diff stats: {:?}", e))?;
//
//        println!("files changed: {}", stats.files_changed());
//
//        if stats.files_changed() > 0 {
//            index.add_all(["*"].iter(), git2::ADD_DEFAULT, None)
//                .map_err(|e| format!("Failed to add files to index: {:?}", e))?;
//            let timestamp_commit_msg = format!("Mona: {}", time::now().asctime());
//            self.commit(&timestamp_commit_msg, &Vec::new())?;
//        }
//
//        // TODO: is this needed?
//        &self.repo.checkout_head(None)
//            .map_err(|e| format!("Failed to checkout head: {:?}", e))?;
//
//        // now need to push to all remotes
//        for remote in self.remotes(&mut sess)?.remotes.iter() {
//            self.push_remote(&remote)?;
//        }
//        Ok(())
//    }
}

#[cfg(test)]
mod test {
    extern crate tempfile;
    extern crate ditto;

    use self::ditto::register::Register;
    use super::*;

    use block::Prim;
    
    #[derive(Debug, PartialEq)]
    struct User {
        name: Register<Prim>,
        age: Register<Prim>
    }

    impl Blockable for User {
        fn blocks(&self) -> Vec<(String, Block)> {
            vec![
                ("$name".into(), Block::Val(self.name.clone())),
                ("$age".into(), Block::Val(self.age.clone())),
            ]
        }
    }

    impl User {
        fn from_db(user_key: &str, db: &DB, mut sess: &mut Session) -> Result<Self> {
            let name_key = format!("users@{}$name", user_key);
            let age_key = format!("users@{}$age", user_key);

            let name = db.read_block(&name_key, &mut sess)?.to_val()?;
            let age = db.read_block(&age_key, &mut sess)?.to_val()?;

            Ok(User {
                name: name,
                age: age
            }) 
        }
    }

    #[test]
    fn init() {
        let dir = tempfile::tempdir().unwrap();
        let mut sess = Session::new(dir.path(), 0);
        sess.create_key_file().unwrap();
        sess.set_pass(":P".as_bytes());
        let git_root = dir.path().join("db");

        let db = DB::init(&git_root, &mut sess).unwrap();
        assert!(git_root.is_dir());

        let key_salt_path = git_root.join("key_salt");
        assert!(key_salt_path.is_file());
        
        let key_salt = db.key_salt(&mut sess).unwrap();
        assert_eq!(key_salt.len(), 256/8);

        let db2 = DB::init(&git_root, &mut sess).unwrap();
        assert_eq!(db2.key_salt(&mut sess).unwrap(), key_salt);

        assert!(db.root.join("cryptic").is_dir());
    }

    #[test]
    fn key_salt_used_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let mut sess = Session::new(dir.path(), 0);
        sess.create_key_file().unwrap();
        sess.set_pass(":P".as_bytes());
        let git_root = &dir.path().join("db");

        let db = DB::init(git_root, &mut sess).unwrap();
        // fix the path salt to "$"
        let encrypted = Plaintext {
            data: "$".as_bytes().to_vec(),
            config: Config::fresh_default().unwrap()
        }.encrypt(&mut sess).unwrap();
            
        encrypted.write(&db.root.join("key_salt"))
            .unwrap();

        let key_salt = db.key_salt(&mut sess).unwrap();
        assert_eq!(key_salt, "$".as_bytes());
        let filepath = db.derive_key_filepath("/a/b/c", &mut sess).unwrap();

        //test vector comes from the python code:
        //>>> import hashlib
        //>>> hashlib.sha256(b"$/a/b/c").hexdigest()
        //'63b2c7879bd2a4d08a4671047a19fdd4c88e580efb66d853045a210eea0afe79'
        let expected = std::path::PathBuf::from("63")
            .join("b2c7879bd2a4d08a4671047a19fdd4c88e580efb66d853045a210eea0afe79");
        assert_eq!(filepath, expected);
    }

    #[test]
    fn read_write_read_block() {
        let dir = tempfile::tempdir().unwrap();
        let mut sess = Session::new(dir.path(), 1);
        sess.create_key_file().unwrap();
        sess.set_pass(":P".as_bytes());
        let git_root = &dir.path().join("db");

        let db = DB::init(git_root, &mut sess).unwrap();

        let bob = User {
            name: Register::new("bob".into(), sess.site_id),
            age: Register::new(1f64.into(), sess.site_id)
        };

        let res = db.read_block("users@bob$name", &mut sess);
        assert!(res.is_err()); // key should not exist
        
        db.write("users@bob", &bob, &mut sess).unwrap();

        let bob_from_db = User::from_db("bob", &db, &mut sess).unwrap();
        assert_eq!(bob_from_db, bob);
    }

    #[test]
    fn sync() {
        use std::io::{Write, stdout};
        println!("Running sync");
        stdout().flush().ok();
        let remote_root_dir = tempfile::tempdir().unwrap();
        let remote_root = remote_root_dir.path();
        let root_a_dir = tempfile::tempdir().unwrap();
        let root_a = root_a_dir.path();
        let root_b_dir = tempfile::tempdir().unwrap();
        let root_b = root_b_dir.path();
        let git_root_a = root_a.join("db");
        let git_root_b = root_b.join("db");

        println!("created temp dirs");

        Repository::init_bare(&remote_root).unwrap();
        
        let mut sess_a = Session::new(&root_a, 1);
        let mut sess_b = Session::new(&root_b, 2);
        println!("created sessions");
        sess_a.create_key_file().unwrap();

        println!("created key_file_a");
        {
            // copy the key_file to the root_b
            let key_file = sess_a.key_file().unwrap();
            let mut f2 = std::fs::File::create(&root_b.join("key_file")).unwrap();
            use std::io::Write;
            f2.write_all(&key_file).unwrap();

            assert_eq!(sess_a.key_file().unwrap(), sess_b.key_file().unwrap());
        }

        println!("copied to key_file_b");
        sess_a.set_pass("secret_pass".as_bytes());
        sess_b.set_pass("secret_pass".as_bytes());

        let db_a = DB::init(&git_root_a, &mut sess_a).unwrap();

        let remote_url = format!("file://{}", remote_root.to_str().unwrap());
        let remote = Remote::no_auth(
            "local_remote".into(),
            &remote_url,
            sess_a.site_id
        );
        
        println!("remote url: '{}'", remote_url);
        db_a.write_remote(&remote, &mut sess_a).unwrap();
        db_a.sync(&mut sess_a).unwrap();

        println!("Finished init of a, a is synced with remote");

        let db_b = DB::init_from_remote(&git_root_b, &remote, &mut sess_b).unwrap();

        assert_eq!(db_a.key_salt(&mut sess_a).unwrap(), db_b.key_salt(&mut sess_b).unwrap());
        
        println!("both db's are initted");
        println!("initial sync");

        // PRE:
        //   create A:users@sam
        //   create B:users@bob
        // POST:
        //   both sites A and B should have same sam and bob entries
        db_a.write(
            "users@sam",
            &User {
                name: Register::new("sam".into(), sess_a.site_id),
                age: Register::new(12.5.into(), sess_a.site_id)
            },
            &mut sess_a
        ).unwrap();
        
        db_b.write(
            "users@bob",
            &User {
                name: Register::new("bob".into(), sess_b.site_id),
                age: Register::new(11.25.into(), sess_b.site_id)
            },
            &mut sess_b
        ).unwrap();

        db_a.sync(&mut sess_a).unwrap();
        db_b.sync(&mut sess_b).unwrap();
        println!("second sync");

        let sam_from_a = User::from_db("sam", &db_a, &mut sess_a).unwrap();
        let sam_from_b = User::from_db("sam", &db_b, &mut sess_b).unwrap();
        assert_eq!(sam_from_a, sam_from_b);

        db_a.sync(&mut sess_a).unwrap();
        db_b.sync(&mut sess_b).unwrap();
        let bob_from_a = User::from_db("bob", &db_a, &mut sess_a).unwrap();
        let bob_from_b = User::from_db("bob", &db_b, &mut sess_b).unwrap();
        assert_eq!(bob_from_a, bob_from_b);

        // PRE:
        //   create A:users@alice (with age 32)
        //   create B:users@alice (with age 32.5)
        // POST:
        //   both sites A and B should converge to the same alice age value
        db_a.write(
            "users@alice",
            &User {
                name: Register::new("alice".into(), sess_a.site_id),
                age: Register::new(32f64.into(), sess_a.site_id)
            },
            &mut sess_a
        ).unwrap();
        
        db_b.write(
            "users@alice",
            &User {
                name: Register::new("alice".into(), sess_b.site_id),
                age: Register::new(32.5.into(), sess_b.site_id)
            },
            &mut sess_b
        ).unwrap();
        
        db_a.sync(&mut sess_a).unwrap();
        db_b.sync(&mut sess_b).unwrap();
        db_a.sync(&mut sess_a).unwrap();

        {
            let alice_from_a = User::from_db("alice", &db_a, &mut sess_a).unwrap();
            let alice_from_b = User::from_db("alice", &db_b, &mut sess_b).unwrap();
            assert_eq!(alice_from_a, alice_from_b);

            let mut alice = User::from_db("alice", &db_b, &mut sess_b).unwrap();
            alice.age.update(33f64.into(), sess_b.site_id);
            db_b.write("users@alice", &alice, &mut sess_b).unwrap();
        }
        
        db_a.sync(&mut sess_a).unwrap();
        db_b.sync(&mut sess_b).unwrap();
        db_a.sync(&mut sess_a).unwrap();

        {
            let alice_from_a = User::from_db("alice", &db_a, &mut sess_a).unwrap();
            let alice_from_b = User::from_db("alice", &db_b, &mut sess_b).unwrap();
            assert_eq!(alice_from_a, alice_from_b);
            assert_eq!(alice_from_a.age.get(), &33f64.into());
        }
    }
}
