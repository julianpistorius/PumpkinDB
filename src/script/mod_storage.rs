// Copyright (c) 2017, All Contributors (see CONTRIBUTORS file)
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.
//!
//! # Storage
//!
//! This module handles all instructions and state related to handling storage
//! capabilities
//!
use lmdb;
use lmdb::traits::LmdbResultExt;
use storage;
use storage::GlobalWriteLock;
use std::mem;
use std::error::Error as StdError;
use std::collections::HashMap;
use super::{Env, EnvId, Module, PassResult, Error, STACK_TRUE, STACK_FALSE, offset_by_size,
            ERROR_EMPTY_STACK, ERROR_INVALID_VALUE, ERROR_DUPLICATE_KEY, ERROR_NO_TX,
            ERROR_UNKNOWN_KEY, ERROR_DATABASE};
use core::ops::Deref;
use byteorder::{BigEndian, WriteBytesExt};
use snowflake::ProcessUniqueId;

pub type CursorId = ProcessUniqueId;

instruction!(WRITE, b"\x85WRITE");
instruction!(WRITE_END, b"\x80\x85WRITE"); // internal instruction

instruction!(READ, b"\x84READ");
instruction!(READ_END, b"\x80\x84READ"); // internal instruction

instruction!(ASSOC, b"\x85ASSOC");
instruction!(ASSOCQ, b"\x86ASSOC?");
instruction!(RETR, b"\x84RETR");

instruction!(CURSOR, b"\x86CURSOR");
instruction!(QCURSOR_FIRST, b"\x8D?CURSOR/FIRST");
instruction!(CURSOR_FIRSTQ, b"\x8DCURSOR/FIRST?");
instruction!(QCURSOR_LAST, b"\x8C?CURSOR/LAST");
instruction!(CURSOR_LASTQ, b"\x8CCURSOR/LAST?");
instruction!(QCURSOR_NEXT, b"\x8C?CURSOR/NEXT");
instruction!(CURSOR_NEXTQ, b"\x8CCURSOR/NEXT?");
instruction!(QCURSOR_PREV, b"\x8C?CURSOR/PREV");
instruction!(CURSOR_PREVQ, b"\x8CCURSOR/PREV?");
instruction!(QCURSOR_SEEK, b"\x8C?CURSOR/SEEK");
instruction!(CURSOR_SEEKQ, b"\x8CCURSOR/SEEK?");
instruction!(QCURSOR_CUR, b"\x8B?CURSOR/CUR");
instruction!(CURSOR_CURQ, b"\x8BCURSOR/CUR?");

instruction!(COMMIT, b"\x86COMMIT");

use std::collections::BTreeMap;

#[derive(PartialEq)]
enum TxType {
    Read, Write
}

pub struct Handler<'a> {
    db: &'a storage::Storage<'a>,
    db_write_txn: Option<(EnvId, lmdb::WriteTransaction<'a>)>,
    db_read_txns: HashMap<EnvId, lmdb::ReadTransaction<'a>>,
    cursors: BTreeMap<(EnvId, Vec<u8>), (TxType, lmdb::Cursor<'a, 'a>)>
}

macro_rules! read_or_write_transaction {
    ($me: expr, $env_id: expr) => {
        if let Some((_, ref txn)) = $me.db_write_txn {
            txn.deref()
        } else if $me.db_read_txns.contains_key(&$env_id) {
            $me.db_read_txns.get(&$env_id).unwrap()
        } else {
            return Err(error_no_transaction!());
        };
    };
}

macro_rules! tx_type {
    ($me: expr, $env_id: expr) => {
        if let Some((_, _)) = $me.db_write_txn {
            TxType::Write
        } else if $me.db_read_txns.contains_key(&$env_id) {
            TxType::Read
        } else {
            return Err(error_no_transaction!());
        };
    };
}

macro_rules! validate_read_lockout {
    ($locks: expr, $env_id: expr) => {
        if $locks.len() > 0 {
            if !$locks.contains_key(&$env_id) {
                return Err(Error::Reschedule)
            }
        }
    };
}

macro_rules! validate_lockout {
    ($env: expr, $name: expr, $pid: expr) => {
        if let Some((pid_, _)) = $name {
            if pid_ != $pid {
                return Err(Error::Reschedule)
            }
        }
    };
}

const STACK_EMPTY_CLOSURE: &'static [u8] = b"";

macro_rules! qcursor_op {
    ($me: expr, $env: expr, $env_id: expr, $op: ident, ($($arg: expr),*)) => {
    {
        validate_read_lockout!($me.db_read_txns, &$env_id);
        validate_lockout!($env, $me.db_write_txn, $env_id);

        let c = stack_pop!($env);

        let txn = read_or_write_transaction!($me, &$env_id);
        let tuple = ($env_id, Vec::from(c));
        let mut cursor = match $me.cursors.remove(&tuple) {
            Some((_, cursor)) => cursor,
            None => return Err(error_invalid_value!(c))
        };
        let access = txn.access();
        let item = cursor.$op::<[u8], [u8]>(&access, $($arg)*);
        match item {
           Ok((key, val)) => {
                let mut offset = 0;
                let sz = key.len() + val.len() + offset_by_size(key.len()) + offset_by_size(val.len());
                let slice = alloc_slice!(sz, $env);
                write_size_into_slice!(key.len(), &mut slice[offset..]);
                offset += offset_by_size(key.len());
                slice[offset..offset + key.len()].copy_from_slice(key);
                offset += key.len();
                write_size_into_slice!(val.len(), &mut slice[offset..]);
                offset += offset_by_size(val.len());
                slice[offset..offset + val.len()].copy_from_slice(val);
                $env.push(slice);
           }
           // not found
           Err(_) => {
                $env.push(STACK_EMPTY_CLOSURE);
           }
        }
        $me.cursors.insert(tuple, (tx_type!($me, &$env_id), cursor));
        Ok(())
    }
    };
}

macro_rules! cursorq_op {
    ($me: expr, $env: expr, $env_id: expr, $op: ident, ($($arg: expr),*)) => {
    {
        validate_read_lockout!($me.db_read_txns, &$env_id);
        validate_lockout!($env, $me.db_write_txn, $env_id);

        let c = stack_pop!($env);

        let txn = read_or_write_transaction!($me, &$env_id);
        let tuple = ($env_id, Vec::from(c));
        let mut cursor = match $me.cursors.remove(&tuple) {
            Some((_, cursor)) => cursor,
            None => return Err(error_invalid_value!(c))
        };
        let access = txn.access();
        let item = cursor.$op::<[u8], [u8]>(&access, $($arg)*);
        match item {
           Ok((_, _)) => {
                $env.push(STACK_TRUE);
           }
           // not found
           Err(_) => {
                $env.push(STACK_FALSE);
           }
        }
        $me.cursors.insert(tuple, (tx_type!($me, &$env_id), cursor));
        Ok(())
    }
    };
}

impl<'a> Module<'a> for Handler<'a> {

    fn done(&mut self, _: &mut Env, pid: EnvId) {
        let is_in_read = self.db_read_txns.contains_key(&pid);
        let mut is_in_write = false;


        if let Some((pid_, _)) = self.db_write_txn {
            is_in_write = pid_ == pid;
        }

        if is_in_read {
            let txn = self.db_read_txns.remove(&pid).unwrap();
            drop(txn)
        }

        if is_in_write {
            match mem::replace(&mut self.db_write_txn, None) {
                Some((_, txn)) => drop(txn),
                None => ()
            }
        }

    }

    fn handle(&mut self, env: &mut Env<'a>, instruction: &'a [u8], pid: EnvId) -> PassResult<'a> {
        try_instruction!(env, self.handle_write(env, instruction, pid));
        try_instruction!(env, self.handle_read(env, instruction, pid));
        try_instruction!(env, self.handle_assoc(env, instruction, pid));
        try_instruction!(env, self.handle_assocq(env, instruction, pid));
        try_instruction!(env, self.handle_retr(env, instruction, pid));
        try_instruction!(env, self.handle_commit(env, instruction, pid));
        try_instruction!(env, self.handle_cursor(env, instruction, pid));
        try_instruction!(env, self.handle_cursor_first(env, instruction, pid));
        try_instruction!(env, self.handle_cursor_next(env, instruction, pid));
        try_instruction!(env, self.handle_cursor_prev(env, instruction, pid));
        try_instruction!(env, self.handle_cursor_last(env, instruction, pid));
        try_instruction!(env, self.handle_cursor_seek(env, instruction, pid));
        try_instruction!(env, self.handle_cursor_cur(env, instruction, pid));
        Err(Error::UnknownInstruction)
    }
}

impl<'a> Handler<'a> {

    pub fn new(db: &'a storage::Storage<'a>) -> Self {
        Handler {
            db: db,
            db_write_txn: None,
            db_read_txns: HashMap::new(),
            cursors: BTreeMap::new()
        }
    }


    #[inline]
    pub fn handle_write(&mut self, env: &mut Env<'a>, instruction: &'a [u8], pid: EnvId) -> PassResult<'a> {
        match instruction {
            WRITE => {
                let v = stack_pop!(env);
                validate_lockout!(env, self.db_write_txn, pid);
                if self.db.try_lock() == false {
                    return Err(Error::Reschedule)
                }
                // prepare transaction
                match lmdb::WriteTransaction::new(self.db.env) {
                    Err(e) => Err(error_database!(e)),
                    Ok(txn) => {
                        self.db_write_txn = Some((pid, txn));
                        env.program.push(WRITE_END);
                        env.program.push(v);
                        Ok(())
                    }
                }
            }
            WRITE_END => {
                validate_lockout!(env, self.db_write_txn, pid);
                self.cursors = mem::replace(&mut self.cursors,
                                            BTreeMap::new()).into_iter()
                    .filter(|t| ((*t).1).0 != TxType::Write).collect();
                self.db_write_txn = None;
                Ok(())
            }
            _ => Err(Error::UnknownInstruction),
        }
    }

    #[inline]
    pub fn handle_read(&mut self, env: &mut Env<'a>, instruction: &'a [u8], pid: EnvId) -> PassResult<'a> {
        match instruction {
            READ => {
                let v = stack_pop!(env);

                validate_read_lockout!(self.db_read_txns, &pid);
                // prepare transaction
                match lmdb::ReadTransaction::new(self.db.env) {
                    Err(e) => Err(error_database!(e)),
                    Ok(txn) => {
                        self.db_read_txns.insert(pid, txn);
                        env.program.push(READ_END);
                        env.program.push(v);
                        Ok(())
                    }
                }
            }
            READ_END => {
                validate_read_lockout!(self.db_read_txns, &pid);
                self.cursors = mem::replace(&mut self.cursors,
                                            BTreeMap::new()).into_iter()
                    .filter(|t| ((*t).1).0 != TxType::Read).collect();
                self.db_read_txns.remove(&pid);
                Ok(())
            }
            _ => Err(Error::UnknownInstruction),
        }
    }

    #[inline]
    pub fn handle_assoc(&mut self, env: &mut Env<'a>, instruction: &'a [u8], pid: EnvId) -> PassResult<'a> {
        if instruction == ASSOC {
            validate_lockout!(env, self.db_write_txn, pid);
            if let Some((_, ref txn)) = self.db_write_txn {
                let value = stack_pop!(env);
                let key = stack_pop!(env);

                let mut access = txn.access();

                match access.put(&self.db.db, key, value, lmdb::put::NOOVERWRITE) {
                    Ok(_) => Ok(()),
                    Err(lmdb::Error::Code(code)) if lmdb::error::KEYEXIST == code => Err(error_duplicate_key!(key)),
                    Err(err) => Err(error_database!(err)),
                }
            } else {
                Err(error_no_transaction!())
            }
        } else {
            Err(Error::UnknownInstruction)
        }
    }

    #[inline]
    pub fn handle_commit(&mut self, _: &Env<'a>, instruction: &'a [u8], pid: EnvId) -> PassResult<'a> {
        if instruction == COMMIT {
            validate_lockout!(env, self.db_write_txn, pid);
            if let Some((_, txn)) = mem::replace(&mut self.db_write_txn, None) {
                match txn.commit() {
                    Ok(_) => Ok(()),
                    Err(reason) => Err(error_database!(reason))
                }
            } else {
                Err(error_no_transaction!())
            }
        } else {
            Err(Error::UnknownInstruction)
        }
    }


    #[inline]
    pub fn handle_retr(&mut self, env: &mut Env<'a>, instruction: &'a [u8], pid: EnvId) -> PassResult<'a> {
        if instruction == RETR {
            validate_lockout!(env, self.db_write_txn, pid);
            validate_read_lockout!(self.db_read_txns, pid);
            let key = stack_pop!(env);

            let txn = read_or_write_transaction!(self, pid);
            let access = txn.access();

            return match access.get::<[u8], [u8]>(&self.db.db, key).to_opt() {
                Ok(Some(val)) => {
                    let slice = alloc_and_write!(val, env);
                    env.push(slice);
                    Ok(())
                }
                Ok(None) => Err(error_unknown_key!(key)),
                Err(err) => Err(error_database!(err)),
            }
        } else {
            Err(Error::UnknownInstruction)
        }
    }

    #[inline]
    pub fn handle_assocq(&mut self, env: &mut Env<'a>, instruction: &'a [u8], pid: EnvId) -> PassResult<'a> {
        if instruction == ASSOCQ {
            validate_lockout!(env, self.db_write_txn, pid);
            let key = stack_pop!(env);

            let txn = read_or_write_transaction!(self, pid);
            let access = txn.access();

            match access.get::<[u8], [u8]>(&self.db.db, key).to_opt() {
                Ok(Some(_)) => {
                    env.push(STACK_TRUE);
                    Ok(())
                }
                Ok(None) => {
                    env.push(STACK_FALSE);
                    Ok(())
                }
                Err(err) => Err(error_database!(err)),
            }
        } else {
            Err(Error::UnknownInstruction)
        }
    }

    fn cast_away(cursor: lmdb::Cursor) -> lmdb::Cursor<'a,'a> {
        unsafe { ::std::mem::transmute(cursor) }
    }

    #[inline]
    pub fn handle_cursor(&mut self, env: &mut Env<'a>, instruction: &'a [u8], pid: EnvId) -> PassResult<'a> {
        if instruction == CURSOR {
            validate_read_lockout!(self.db_read_txns, pid);
            validate_lockout!(env, self.db_write_txn, pid);

            let txn = read_or_write_transaction!(self, pid);
            match txn.cursor(&self.db.db) {
                Ok(cursor) => {
                    let id = CursorId::new();
                    let mut bytes = Vec::new();
                    if cfg!(target_pointer_width = "64") {
                        let _ = bytes.write_u64::<BigEndian>(id.prefix as u64);
                    }
                    if cfg!(target_pointer_width = "32") {
                        let _ = bytes.write_u32::<BigEndian>(id.prefix as u32);
                    }
                    let _ = bytes.write_u64::<BigEndian>(id.offset);
                    self.cursors.insert((pid.clone(), bytes.clone()), (tx_type!(self, pid), Handler::cast_away(cursor)));
                    let slice = alloc_and_write!(bytes.as_slice(), env);
                    env.push(slice);
                    Ok(())
                },
                Err(err) => Err(error_database!(err))
            }
        } else {
            Err(Error::UnknownInstruction)
        }
    }

    #[inline]
    pub fn handle_cursor_first(&mut self, env: &mut Env<'a>, instruction: &'a [u8], pid: EnvId) -> PassResult<'a> {
        if instruction == QCURSOR_FIRST {
            qcursor_op!(self, env, pid, first, ())
        } else if instruction == CURSOR_FIRSTQ {
            cursorq_op!(self, env, pid, first, ())
        } else {
            Err(Error::UnknownInstruction)
        }
    }


    #[inline]
    pub fn handle_cursor_next(&mut self, env: &mut Env<'a>, instruction: &'a [u8], pid: EnvId) -> PassResult<'a> {
        if instruction == QCURSOR_NEXT {
            qcursor_op!(self, env, pid, next, ())
        } else if instruction == CURSOR_NEXTQ {
            cursorq_op!(self, env, pid, next, ())
        } else {
            Err(Error::UnknownInstruction)
        }
    }

    #[inline]
    pub fn handle_cursor_prev(&mut self, env: &mut Env<'a>, instruction: &'a [u8], pid: EnvId) -> PassResult<'a> {
        if instruction == QCURSOR_PREV {
            qcursor_op!(self, env, pid, prev, ())
        } else if instruction == CURSOR_PREVQ {
            cursorq_op!(self, env, pid, prev, ())
        } else {
            Err(Error::UnknownInstruction)
        }
    }

    #[inline]
    pub fn handle_cursor_last(&mut self, env: &mut Env<'a>, instruction: &'a [u8], pid: EnvId) -> PassResult<'a> {
        if instruction == QCURSOR_LAST {
            qcursor_op!(self, env, pid, last, ())
        } else if instruction == CURSOR_LASTQ {
            cursorq_op!(self, env, pid, last, ())
        } else {
            Err(Error::UnknownInstruction)
        }
    }

    #[inline]
    pub fn handle_cursor_seek(&mut self, env: &mut Env<'a>, instruction: &'a [u8], pid: EnvId) -> PassResult<'a> {
        if instruction == QCURSOR_SEEK {
            let key = stack_pop!(env);

            qcursor_op!(self, env, pid, seek_range_k, (key))
        } else if instruction == CURSOR_SEEKQ {
            let key = stack_pop!(env);

            cursorq_op!(self, env, pid, seek_range_k, (key))
        } else {
            Err(Error::UnknownInstruction)
        }
    }

    #[inline]
    pub fn handle_cursor_cur(&mut self, env: &mut Env<'a>, instruction: &'a [u8], pid: EnvId) -> PassResult<'a> {
        if instruction == QCURSOR_CUR {
            qcursor_op!(self, env, pid, get_current, ())
        } else if instruction == CURSOR_CURQ {
            cursorq_op!(self, env, pid, get_current, ())
        } else {
            Err(Error::UnknownInstruction)
        }
    }
}

#[cfg(test)]
#[allow(unused_variables, unused_must_use, unused_mut, unused_imports)]
mod tests {
    use script::{Env, Scheduler, Error, RequestMessage, ResponseMessage, EnvId, parse, offset_by_size};
    use byteorder::WriteBytesExt;
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::fs;
    use tempdir::TempDir;
    use lmdb;
    use crossbeam;
    use script::binparser;
    use pubsub;
    use storage;
    use timestamp;

    const _EMPTY: &'static [u8] = b"";

    #[test]
    fn errors_during_txn() {
        eval!("[[\"Hey\" ASSOC COMMIT] WRITE] TRY [\"Hey\" ASSOC?] READ",
              env,
              result,
              {
                  assert_eq!(Vec::from(env.pop().unwrap()), parsed_data!("0x00"));
              });
        eval!("[[\"Hey\" ASSOC COMMIT] WRITE] TRY DROP [\"Hey\" \"there\" ASSOC COMMIT] WRITE [\"Hey\" ASSOC?] READ",
              env,
              result,
              {
                  assert_eq!(Vec::from(env.pop().unwrap()), parsed_data!("0x01"));
              });

    }

    use test::Bencher;

    #[bench]
    fn write_1000_kv_pairs_in_isolated_txns(b: &mut Bencher) {
        bench_eval!("[HLC \"Hello\"] 1000 TIMES [[ASSOC COMMIT] WRITE] 1000 TIMES", b);
    }

    #[bench]
    fn write_1000_kv_pairs_in_isolated_txns_baseline(b: &mut Bencher) {
        let dir = TempDir::new("pumpkindb").unwrap();
        let path = dir.path().to_str().unwrap();
        fs::create_dir_all(path).expect("can't create directory");
        let env = unsafe {
            let mut builder = lmdb::EnvBuilder::new().expect("can't create env builder");
            builder.set_mapsize(1024 * 1024 * 1024).expect("can't set mapsize");
            builder.open(path, lmdb::open::NOTLS, 0o600).expect("can't open env")
        };
        let timestamp = timestamp::Timestamp::new(None);
        let db = storage::Storage::new(&env);
        b.iter(move || {
            let mut timestamps = Vec::new();
            for i in 0..1000 {
                timestamps.push(timestamp.hlc());
            }
            for ts in timestamps {
                let txn = lmdb::WriteTransaction::new(db.env).unwrap();
                {
                    let mut access = txn.access();
                    let mut key: Vec<u8> = Vec::new();

                    ts.write_bytes(&mut key);

                    let _ = access.put(&db.db, key.as_slice(), "Hello".as_bytes(), lmdb::put::NOOVERWRITE).unwrap();
                }
                let _ = txn.commit().unwrap();
            }
        });
    }

}
