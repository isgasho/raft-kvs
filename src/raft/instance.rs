//! Define Raft instance

use crate::raft::event::RaftEvent;
use crate::raft::log::Log;
use crate::raft::rpc::{
    AppendEntries, AppendEntriesReply, MockRPCService, RaftRPC, RequestVote, RequestVoteReply,
};
use rand::Rng;
use slog::info;
use std::collections::HashMap;

/// Raft role
#[derive(PartialEq, Debug)]
pub enum Role {
    Follower,
    Candidate,
    Leader,
}

/// A Raft instance
pub struct Raft {
    /// latest term server has seen (initialized to 0 on first boot)
    /// This is a persistent state.
    pub current_term: i64,
    /// candidate_id that received vote in current term
    /// This is a persistent state.
    pub voted_for: Option<u64>,
    /// log entries; each entry contains command for state machine, and term when
    /// entry was received by leader (first index is 1)
    /// This is a persistent state.
    pub log: Vec<(i64, Log)>,

    /// index of highest log entry known to be committed (initialized to 0)
    /// This is a volatile state.
    pub commit_index: i64,
    /// index of highest log entry applied to state machine (initialized to 0)
    /// This is a volatile state.
    pub last_applied: i64,

    /// for each server, index of the next log entry to send to that serve
    /// (initialized to leader last log index + 1)
    /// This is a volatile state for leaders.
    pub next_index: HashMap<u64, i64>,
    /// for each server, index of highest log entry known to be replicated on server
    ///(initialized to 0)
    /// This is a volatile state for leaders.
    pub match_index: HashMap<u64, i64>,

    /// RPC service for Raft
    /// This is raft-kvs internal state.
    pub rpc: MockRPCService,
    /// role of Raft instance
    /// This is raft-kvs internal state.
    pub role: Role,
    /// known peers, must include `self_id`
    /// This is raft-kvs internal state.
    pub known_peers: Vec<u64>,
    /// id of myself
    /// This is raft-kvs internal state.
    pub id: u64,

    /// follower will start election after this time
    /// This is raft-kvs internal state. Should be reset when become follower.
    election_start_at: u64,

    /// candidate fails election after this time, and starts new election
    /// This is raft-kvs internal state. Should be reset when become candidate.
    election_timeout_at: u64,

    /// number of votes a candidate gets
    /// This is raft-kvs internal state. Should be reset when become candidate.
    vote_from: HashMap<u64, ()>,

    /// logger
    /// This is raft-kvs internal state.
    logger: slog::Logger,
}

impl Raft {
    /// create new raft instance
    pub fn new(known_peers: Vec<u64>, logger: slog::Logger, id: u64) -> Self {
        Raft {
            current_term: 0,
            voted_for: None,
            log: vec![],
            commit_index: 0,
            last_applied: 0,
            next_index: HashMap::new(),
            match_index: HashMap::new(),
            rpc: MockRPCService::new(logger.clone(), id),
            role: Role::Follower,
            election_start_at: 0,
            election_timeout_at: 0,
            known_peers,
            id,
            vote_from: HashMap::new(),
            logger,
        }
    }

    /// timer tick
    /// `current_tick` is current system time
    pub fn tick(&mut self, current_tick: u64) {
        match self.role {
            Role::Follower => {
                if current_tick > self.election_start_at {
                    self.become_candidate(current_tick);
                }
            }
            Role::Candidate => {
                if current_tick > self.election_timeout_at {
                    self.begin_election(current_tick);
                }
            }
            Role::Leader => {
                self.heartbeats();
            }
        }
    }

    /// send heartbeats to followers
    fn heartbeats(&mut self) {
        for peer in self.known_peers.clone().iter() {
            let peer = *peer;
            if peer != self.id {
                self.heartbeat(peer);
            }
        }
    }

    /// send heartbeat to peer
    fn heartbeat(&mut self, peer: u64) {
        let entries = vec![];
        self.rpc.send(
            peer,
            AppendEntries {
                term: self.current_term,
                leader_id: self.id,
                prev_log_term: self.last_log_term(),
                prev_log_index: self.last_log_index(),
                entries,
                leader_commit: self.commit_index,
            }
            .into(),
        );
    }

    /// sync log to peer
    fn sync_log_with(&mut self, peer: u64) {
        let next_idx = *self.next_index.get(&peer).unwrap();
        if self.last_log_index() < next_idx {
            return;
        }
        self.rpc.send(
            peer,
            AppendEntries {
                term: self.current_term,
                leader_id: self.id,
                prev_log_term: self.log_term_of(next_idx - 1),
                prev_log_index: next_idx - 1,
                entries: vec![self.log[next_idx as usize - 1].clone()],
                leader_commit: self.commit_index,
            }
            .into(),
        );
    }

    /// become a follower
    fn become_follower(&mut self, current_tick: u64) {
        self.role = Role::Follower;
        self.election_start_at = current_tick + Self::tick_election_start_at();
    }

    /// become a candidate
    fn become_candidate(&mut self, current_tick: u64) {
        info!(self.logger, "role transition"; "role" => format!("{:?}->{:?}", self.role, Role::Candidate));
        self.current_term += 1;
        self.role = Role::Candidate;
        self.begin_election(current_tick);
    }

    // become a leader
    fn become_leader(&mut self) {
        self.role = Role::Leader;
        // initialize leader-related data structure
        self.match_index = HashMap::new();
        self.next_index = HashMap::new();
        for peer in self.known_peers.iter() {
            let peer = *peer;
            if peer != self.id {
                self.match_index.insert(peer, 0);
                self.next_index.insert(peer, self.last_log_index() + 1);
            }
        }
        // send heartbeats to followers
        self.heartbeats();
    }

    /// begin election
    fn begin_election(&mut self, current_tick: u64) {
        self.voted_for = Some(self.id);
        // TODO: persistent
        self.vote_from = HashMap::new();
        self.vote_from.insert(self.id, ());
        self.election_timeout_at = current_tick + Self::tick_election_fail_at();
        for peer in self.known_peers.iter() {
            let peer = *peer;
            if peer != self.id {
                self.rpc.send(
                    peer,
                    RequestVote {
                        term: self.current_term,
                        candidate_id: self.id,
                        last_log_index: self.last_log_index(),
                        last_log_term: self.last_log_term(),
                    }
                    .into(),
                );
            }
        }
    }

    /// get last log index
    /// returns 0 if there's no log
    fn last_log_index(&self) -> i64 {
        self.log.len() as i64
    }

    /// get last log term
    /// returns 0 if there's no log
    fn last_log_term(&self) -> i64 {
        self.log_term_of(self.last_log_index())
    }

    /// get term of log given id
    fn log_term_of(&self, id: i64) -> i64 {
        if id == 0 {
            0
        } else {
            self.log[id as usize - 1].0 as i64
        }
    }

    /// candidate rpc event
    fn candidate_rpc_event(&mut self, from: u64, event: RaftRPC, current_tick: u64) {
        match event {
            RaftRPC::RequestVoteReply(reply) => {
                if reply.vote_granted {
                    self.vote_from.insert(from, ());
                    if self.vote_from.len() * 2 >= self.known_peers.len() {
                        self.become_leader();
                    }
                }
            }
            RaftRPC::AppendEntries(request) => {
                if request.term == self.current_term {
                    self.become_follower(current_tick);
                    return;
                }
            }
            _ => {}
        }
    }

    /// follower rpc event
    fn follower_rpc_event(&mut self, from: u64, event: RaftRPC, current_tick: u64) {
        match event {
            RaftRPC::RequestVote(request) => {
                if request.term < self.current_term {
                    self.rpc.send(
                        from,
                        RequestVoteReply {
                            term: self.current_term,
                            vote_granted: false,
                        }
                        .into(),
                    );
                    return;
                }
                let mut vote_granted = match self.voted_for {
                    Some(candidate_id) => candidate_id == request.candidate_id,
                    None => true,
                };
                // TODO: how to check up-to-date?
                if request.last_log_index < self.last_log_index() {
                    vote_granted = false;
                }
                if vote_granted {
                    self.voted_for = Some(request.candidate_id);
                }
                self.rpc.send(
                    from,
                    RequestVoteReply {
                        term: self.current_term,
                        vote_granted,
                    }
                    .into(),
                );
            }
            RaftRPC::AppendEntries(request) => {
                let mut ok = false;
                if request.term < self.current_term {
                    info!(self.logger, "append entries failed"; "reason" => "lower term");
                } else if request.prev_log_index > self.last_log_index() {
                    info!(self.logger, "append entries failed"; "reason" => "log not found");
                } else {
                    if self.log_term_of(request.prev_log_index) == request.prev_log_term {
                        ok = true;
                    } else {
                        info!(self.logger, "append entries failed"; "reason" => "term not match");
                    }
                }
                if ok {
                    let length = request.entries.len();
                    self.log.drain(request.prev_log_index as usize..);
                    self.log.extend(request.entries);
                    info!(self.logger, "append entries success";
                            "entries_processed" => length,
                            "log_length" => self.log.len());
                }
                self.rpc
                    .send(from, AppendEntriesReply::new(self.current_term, ok).into());
            }
            _ => {}
        }
    }

    /// process Raft event
    pub fn on_event(&mut self, event: RaftEvent, current_tick: u64) {
        match event {
            RaftEvent::RPC((from, event)) => {
                if event.term() > self.current_term {
                    self.become_follower(current_tick);
                }
                match self.role {
                    Role::Follower => self.follower_rpc_event(from, event, current_tick),
                    Role::Candidate => self.candidate_rpc_event(from, event, current_tick),
                    Role::Leader => {}
                }
            }
            _ => unimplemented!(),
        }
    }

    /// append log
    pub fn append_log(&mut self, log: Log) {
        self.log.push((self.current_term, log));
        for peer in self.known_peers.clone().iter() {
            let peer = *peer;
            if peer != self.id {
                self.sync_log_with(peer);
            }
        }
    }

    /// generate random election timeout
    fn tick_election_fail_at() -> u64 {
        rand::thread_rng().gen_range(200, 300)
    }

    /// generate random election start
    fn tick_election_start_at() -> u64 {
        rand::thread_rng().gen_range(200, 300)
    }
}
