use crate::types::{ControlMessage, LeaderTimer, Log, Message, Peer, State};
use crossbeam::channel::Receiver;
use crossbeam_channel::Select;
use rand::Rng;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

const LEADER_TIMEOUT: u64 = 500;
const NOT_LEADER_MIN_TIMEOUT: u64 = 2000;
const NOT_LEADER_MAX_TIMEOUT: u64 = 3500;

pub struct Replica {
    // This is simply the value the consistency of which the consensus
    // maintains.
    value: i32,
    // ID of this replica.
    id: usize,
    // Current term.
    current_term: usize,
    // ID of peers with votes for self.
    current_votes: Option<Box<BTreeSet<usize>>>,
    // Receiving end of a multiple producer single consumer channel for the Raft
    // protocol.
    rx: Receiver<Message>,
    // Receiving end of a channel for forced state change messages.
    rx_control: Receiver<ControlMessage>,
    // State of this replica.
    state: State,
    // State before dying.
    prev_state: State,
    // Vector of peers, i.e. their IDs and the corresponding transmission ends
    // of mpsc channels.
    peers: Vec<Peer>,
    // Who the last vote was cast for.
    voted_for: Option<usize>,
    // Logs are simply the terms when the corresponding command was received by
    // the then-leader.
    log: Vec<Log>,
    // Index of highest log entry known to be committed.
    commit_index: usize,
    // Index of highest log entry applied to the state machine.
    last_applied: usize,
    // For each server, index of the next log entry to send to that server. Only
    // present on leaders.
    next_index: BTreeMap<usize, usize>,
    // For each server, index of highest log entry known to be replicated on
    // that server. Only present on leaders.
    match_index: BTreeMap<usize, usize>,
    // Timer to times heartbeat messages on the leader.
    leader_timer: LeaderTimer,
}

impl Replica {
    // This function starts the replica and blocks forever.
    pub fn start(
        id: usize,
        rx: Receiver<Message>,
        rx_control: Receiver<ControlMessage>,
        peers: Vec<Peer>,
    ) {
        let mut replica = Replica {
            value: 0,
            id: id,
            current_term: 0,
            current_votes: None,
            rx: rx,
            rx_control: rx_control,
            state: State::Follower,
            prev_state: State::Dead,
            peers: peers,
            voted_for: None,
            log: vec![
                Log {
                    delta: 0,
                    term: 0,
                    index: 0
                };
                1
            ],
            commit_index: 0,
            last_applied: 0,
            next_index: BTreeMap::new(),
            match_index: BTreeMap::new(),
            leader_timer: LeaderTimer::new(Duration::from_millis(LEADER_TIMEOUT)),
        };

        replica.poll();
    }

    fn send_message(&self, peer_id: usize, message: Message) {
        self.get_peer_by_id(peer_id).send(message);
    }

    fn broadcast_message<F>(&self, message_generator: F)
    where
        F: Fn(&Peer) -> Message,
    {
        for peer in &self.peers {
            peer.send(message_generator(peer));
        }
    }

    fn read_message_with_timeout(
        &self,
        timeout: Duration,
    ) -> (Option<Message>, Option<ControlMessage>) {
        let mut select = Select::new();
        let (oper1, oper2) = (select.recv(&self.rx), select.recv(&self.rx_control));
        match select.select_timeout(timeout) {
            Ok(oper) => match oper.index() {
                i if i == oper1 => match oper.recv(&self.rx) {
                    Ok(msg) => (Some(msg), None),
                    Err(_) => panic!("unexpected error"),
                },
                i if i == oper2 => match oper.recv(&self.rx_control) {
                    Ok(msg) => (None, Some(msg)),
                    Err(_) => panic!("unexpected error"),
                },
                _ => unreachable!(),
            },
            _ => (None, None),
        }
    }

    fn get_entries_for_peer(&self, peer_id: usize) -> Vec<Log> {
        // println!(
        // "my logs {:?}, next index for peer {}",
        // self.log, self.next_index[&peer_id]
        // );
        (&self.log[self.next_index[&peer_id]..self.log.len()]).to_vec()
    }

    fn poll(&mut self) {
        let mut rng = rand::thread_rng();
        loop {
            match self.state {
                State::Leader => {
                    if self.leader_timer.fired() {
                        // println!("broadcasting append entries");
                        for peer in &self.peers {
                            // println!(
                            // "entries for peer {}: {:?}",
                            // peer.id,
                            // self.get_entries_for_peer(peer.id)
                            // );
                        }
                        self.broadcast_message(|p: &Peer| Message::AppendEntryRequest {
                            term: self.current_term,
                            from_id: self.id,
                            prev_log_index: self.next_index[&p.id] - 1,
                            prev_log_term: self.log[self.next_index[&p.id] - 1].term,
                            entries: self.get_entries_for_peer(p.id),
                            commit_index: self.commit_index,
                        });
                        self.leader_timer.renew();
                    }

                    let timeout = Duration::from_millis(LEADER_TIMEOUT);
                    let messages = self.read_message_with_timeout(timeout);
                    match messages {
                        (Some(msg), None) => self.process_message_as_leader(msg),
                        (None, Some(msg)) => self.process_control_message(msg),
                        (None, None) => {}
                        (_, _) => unreachable!(),
                    };
                }
                State::Follower => {
                    let timeout = Duration::from_millis(
                        rng.gen_range(NOT_LEADER_MIN_TIMEOUT..=NOT_LEADER_MAX_TIMEOUT),
                    );
                    let messages = self.read_message_with_timeout(timeout);
                    match messages {
                        (None, None) => self.become_candidate(),
                        (Some(msg), None) => self.process_message_as_follower(msg),
                        (None, Some(msg)) => self.process_control_message(msg),
                        (_, _) => unreachable!(),
                    };
                }
                State::Candidate => {
                    let timeout = Duration::from_millis(
                        rng.gen_range(NOT_LEADER_MIN_TIMEOUT..=NOT_LEADER_MAX_TIMEOUT),
                    );
                    let messages = self.read_message_with_timeout(timeout);
                    match messages {
                        (None, None) => self.become_candidate(),
                        (Some(msg), None) => self.process_message_as_candidate(msg),
                        (None, Some(msg)) => self.process_control_message(msg),
                        (_, _) => unreachable!(),
                    };
                }
                State::Dead => {
                    let message = self.rx_control.recv().unwrap();
                    self.process_control_message(message);
                }
            }

            self.apply_ready_changes();
        }
    }

    fn append(&mut self, delta: i32) {
        self.log.push(Log {
            index: self.log.len(),
            delta: delta,
            term: self.current_term,
        });
    }

    fn apply_ready_changes(&mut self) {
        // Move the commit index to the latest log index that has been replicated
        // on the majority of the replicas.
        if self.state == State::Leader && self.commit_index < self.log.len() - 1 {
            let mut n = self.log.len() - 1;
            while n > self.commit_index {
                let num_replications =
                    self.match_index
                        .iter()
                        .fold(0, |acc, mtch_idx| if mtch_idx.1 >= &n { acc + 1 } else { acc });

                if num_replications * 2 >= self.peers.len() && self.log[n].term == self.current_term
                {
                    println!("match index {:?} log {:?}", self.match_index, self.log);
                    println!(
                        "counting replications: {}, setting commit index to {}",
                        num_replications, n
                    );
                    self.commit_index = n;
                }
                n -= 1;
            }
        }

        // Apply changes that are behind the currently committed index.
        while self.commit_index > self.last_applied {
            println!(
                "applying change on {:?} {} commit index {} last applied {}->{} log {:?}",
                self.state,
                self.id,
                self.commit_index,
                self.last_applied,
                self.last_applied + 1,
                self.log
            );
            self.last_applied += 1;
            self.value += self.log[self.last_applied].delta;
        }
        println!(
            "i am {:?} {} and my value is {} with commit index {} and log {:?} and match index {:?}",
            self.state, self.id, self.value, self.commit_index, self.log, self.match_index
        );
    }

    fn process_control_message(&mut self, message: ControlMessage) {
        match message {
            ControlMessage::Up => self.become_alive(),
            ControlMessage::Down => self.become_dead(),
            ControlMessage::Apply(delta) => match self.state {
                State::Leader => self.append(delta),
                _ => {}
            },
        }
    }

    fn process_message_as_leader(&mut self, message: Message) {
        match message {
            Message::AppendEntryResponse {
                from_id,
                success,
                term,
                last_index,
            } => {
                if term > self.current_term {
                    println!("i {} thought i was leader at term {} got term {} from {} becoming follower", self.id, self.current_term, term, from_id);
                    self.become_follower(term);
                } else if success {
                    self.next_index.insert(from_id, last_index + 1);
                    self.match_index.insert(from_id, last_index);
                } else {
                    self.next_index
                        .insert(from_id, self.next_index[&from_id] - 1);
                }
            }
            _ => {}
        }
    }

    fn process_request_vote_request_as_follower(
        &mut self,
        from_id: usize,
        term: usize,
        last_log_index: usize,
        last_log_term: usize,
    ) {
        if self.current_term > term {
            self.send_message(
                from_id,
                Message::RequestVoteResponse {
                    from_id: self.id,
                    term: self.current_term,
                    vote_granted: false,
                },
            );
        } else if self.current_term < term {
            self.become_follower(term);
        }

        if self.voted_for == None || self.voted_for == Some(from_id) {
            if self.log[self.log.len() - 1].index <= last_log_index
                && self.log[self.log.len() - 1].term <= last_log_term
            {
                self.send_message(
                    from_id,
                    Message::RequestVoteResponse {
                        from_id: self.id,
                        term: self.current_term,
                        vote_granted: true,
                    },
                )
            } else {
                self.send_message(
                    from_id,
                    Message::RequestVoteResponse {
                        from_id: self.id,
                        term: self.current_term,
                        vote_granted: false,
                    },
                );
            }
        } else {
            self.send_message(
                from_id,
                Message::RequestVoteResponse {
                    from_id: self.id,
                    term: self.current_term,
                    vote_granted: false,
                },
            );
        }
    }

    fn process_append_entry_request_as_follower(
        &mut self,
        from_id: usize,
        term: usize,
        prev_log_index: usize,
        prev_log_term: usize,
        entries: Vec<Log>,
        commit_index: usize,
    ) {
        if entries.len() != 0 {
            println!("received non empty entries prev log index {} self log len {} prev term {} self prev term {}", prev_log_index, self.log.len(), prev_log_term, self.log[prev_log_index].term);
        }
        // Check that the leader's term is at least as large as ours.
        if self.current_term > term {
            println!(
                "peer {} is follower and received term {} from {} is smaller than self term {}",
                self.id, term, from_id, self.current_term
            );
            self.send_message(
                from_id,
                Message::AppendEntryResponse {
                    from_id: self.id,
                    term: self.current_term,
                    success: false,
                    last_index: self.log.len() - 1,
                },
            );
            return;
        // If our log doesn't contain an entry at prev_log_index with
        // the prev_log_term term, reply false.
        } else if prev_log_index >= self.log.len() || self.log[prev_log_index].term != prev_log_term
        {
            self.send_message(
                from_id,
                Message::AppendEntryResponse {
                    from_id: self.id,
                    term: self.current_term,
                    success: false,
                    last_index: self.log.len() - 1,
                },
            );
            return;
        }

        if entries.len() != 0 {
            println!("can append entries");
        }

        for entry in entries {
            if entry.index < self.log.len() && entry.term != self.log[entry.index].term {
                self.log.truncate(entry.index);
            }

            if entry.index == self.log.len() {
                self.log.push(entry);
            }
        }

        if commit_index > self.commit_index && self.log.len() != 0 {
            self.commit_index = if commit_index < self.log[self.log.len() - 1].index {
                commit_index
            } else {
                self.log[self.log.len() - 1].index
            }
        }

        println!(
            "my commit index as follower {}: {}",
            self.id, self.commit_index
        );

        println!("{:?} {} responding success to {}", self.state, self.id, from_id);
        self.send_message(
            from_id,
            Message::AppendEntryResponse {
                from_id: self.id,
                term: self.current_term,
                success: true,
                last_index: self.log.len() - 1,
            },
        );
    }

    fn process_message_as_follower(&mut self, message: Message) {
        match message {
            Message::RequestVoteRequest {
                from_id,
                term,
                last_log_index,
                last_log_term,
            } => self.process_request_vote_request_as_follower(
                from_id,
                term,
                last_log_index,
                last_log_term,
            ),
            Message::AppendEntryRequest {
                term,
                from_id,
                prev_log_index,
                prev_log_term,
                entries,
                commit_index,
            } => self.process_append_entry_request_as_follower(
                from_id,
                term,
                prev_log_index,
                prev_log_term,
                entries,
                commit_index,
            ),
            Message::AppendEntryResponse { .. } => { /* ignore */ }
            Message::RequestVoteResponse { .. } => { /* ignore */ }
        }
    }

    fn get_peer_by_id(&self, peer_id: usize) -> &Peer {
        &self.peers[self
            .peers
            .binary_search_by_key(&peer_id, |peer| peer.id)
            .unwrap()]
    }

    fn process_message_as_candidate(&mut self, message: Message) {
        match message {
            Message::AppendEntryRequest { term, from_id, .. } => {
                if term >= self.current_term {
                    self.become_follower(term);
                    self.process_message_as_follower(message);
                } else {
                    println!("peer {} is candidate and received term {} from {} is smaller than self term {}", self.id, term, from_id, self.current_term);
                    self.send_message(
                        from_id,
                        Message::AppendEntryResponse {
                            from_id: self.id,
                            term: self.current_term,
                            success: false,
                            last_index: self.log.len() - 1,
                        },
                    )
                }
            }
            Message::RequestVoteRequest { term, from_id, .. } => {
                if term > self.current_term {
                    self.become_follower(term);
                    self.process_message_as_follower(message);
                } else {
                    self.send_message(
                        from_id,
                        Message::RequestVoteResponse {
                            from_id: self.id,
                            term: self.current_term,
                            vote_granted: false,
                        },
                    );
                }
            }
            Message::RequestVoteResponse {
                from_id,
                term,
                vote_granted,
            } => {
                if term > self.current_term {
                    self.become_follower(term);
                } else if vote_granted {
                    if let Some(cur_votes) = &mut self.current_votes {
                        cur_votes.insert(from_id);
                        if cur_votes.len() * 2 > self.peers.len() + 1 {
                            self.become_leader();
                        }
                    }
                }
            }
            Message::AppendEntryResponse { .. } => { /* ignore */ }
        }
    }

    fn become_alive(&mut self) {
        println!("peer {} becoming alive", self.id);
        if self.prev_state == State::Dead {
            self.become_follower(0);
        } else {
            self.state = self.prev_state;
        }
        self.prev_state = State::Dead;
    }

    fn become_dead(&mut self) {
        println!("peer {} becoming dead", self.id);
        self.prev_state = self.state;
        self.state = State::Dead;
    }

    fn become_leader(&mut self) {
        println!(
            "peer {} is now leader with term {}",
            self.id, self.current_term
        );
        self.state = State::Leader;
        self.current_votes = None;
        self.voted_for = None;
        self.next_index = BTreeMap::new();
        self.match_index = BTreeMap::new();
        for peer in &self.peers {
            self.next_index.insert(peer.id, self.log.len());
            self.match_index.insert(peer.id, 0);
        }
    }

    fn become_follower(&mut self, term: usize) {
        println!("peer {} is now follower with term {}", self.id, term);
        self.current_term = term;
        self.state = State::Follower;
        self.current_votes = None;
        self.voted_for = None;
    }

    fn become_candidate(&mut self) {
        // Increase current term.
        self.current_term += 1;
        println!("peer {} is candidate term {}", self.id, self.current_term);
        // Claim yourself a candidate.
        self.state = State::Candidate;
        // Initialize votes. Vote for yourself.
        let mut votes = BTreeSet::new();
        votes.insert(self.id);
        self.current_votes = Some(Box::new(votes));
        self.voted_for = Some(self.id);
        // Fan out vote requests.
        self.broadcast_message(|_: &Peer| Message::RequestVoteRequest {
            from_id: self.id,
            term: self.current_term,
            last_log_index: self.log.len() - 1,
            last_log_term: self.log[self.log.len() - 1].term,
        });
    }
}
