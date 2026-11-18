//! The loss model: runs of arrivals and of erasures over the symbols in the order they were
//! sent, each run's length drawn from its own measured distribution (a two-state semi-Markov
//! chain), fitted to the reported outcomes, and from it the exact chance a block loses more
//! than its repairs. A two-state Markov chain gives every run a geometric length; the path's
//! runs, queues and stalls among them, have heavier tails, and in simulation the chain
//! underestimated rare misses twelve-fold.
//!
//! The model is read in send order across every lane, not lane by lane. The path erases in
//! the order packets cross it, so a burst of loss falls on symbols sent one after another,
//! whichever lanes carry them; and on a stream lane a loss stalls every later symbol on that
//! lane for a resend's round trip, so one burst stalls the few lanes it fell on together, and
//! the frames after it lose their symbols on those lanes together. Lanes taken as independent
//! chains miss that, and in simulation underestimated misses up to twenty-fold; the chain in
//! send order sees both as runs of erasures. A frame's symbols are sent in a row, so its chance
//! of a miss is the chain's over n symbols in a row, from the long-run state: the sender
//! cannot know the state when a frame starts, since what it hears is a round trip old.

use std::collections::VecDeque;
use std::num::NonZeroUsize;

/// How many lanes a session deals its frames over: at least 1, at most what a 16-bit lane
/// index numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Lanes(u16);

impl Lanes {
    /// One lane.
    pub const ONE: Lanes = Lanes(1);

    /// `n` lanes, if there is at least one.
    pub fn new(n: u16) -> Option<Lanes> {
        (n > 0).then_some(Lanes(n))
    }

    pub fn get(self) -> usize {
        self.0 as usize
    }

    /// As many lanes as `n`, within what a lane index numbers.
    pub fn at_most(n: NonZeroUsize) -> Lanes {
        Lanes(n.get().min(u16::MAX as usize) as u16)
    }

    /// The lane piece `i` of a block goes on, its first on lane `first`. Pieces are dealt to
    /// the lanes in turn, in the order they leave, block after block ([`super::link::Link`]
    /// carries the turn from one block to the next and starts it over at the first lane when
    /// the lanes become fewer than it reached): a lane's pieces are as far apart in send order as
    /// there are lanes, so a stream lane stalled by a loss holds back none of the pieces that
    /// leave during the stall when the lanes outnumber them ([`super::link::Link::wanted`]).
    pub fn lane(self, first: usize, i: usize) -> usize {
        (first + i) % self.0 as usize
    }
}

/// What became of one symbol: the model's two states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrival {
    /// It arrived by its frame's deadline.
    Arrived,
    /// It did not.
    Erased,
}

/// Erasures as runs: arrivals come in runs whose lengths follow one distribution, erasures in
/// runs that follow another, one after the other (a two-state semi-Markov chain).
///
/// For each state, `survive[ℓ − 1]` is the chance a run lasts at least ℓ symbols, as measured,
/// up to the longest run seen, L. Past L nothing was measured, and a run that has lasted L
/// goes on with chance `on` each symbol: memoryless, the geometric being the distribution of
/// the most entropy for its mean, the least assumed, with `on` by Laplace's rule over every
/// symbol of the state seen ([`Runs::of`]). Its ages from L on are one state of the recursion,
/// since none of them remembers more than another.
#[derive(Clone, Debug, PartialEq)]
pub struct Runs {
    survive: [Vec<f64>; 2],
    on: [f64; 2],
}

impl Runs {
    /// The runs that ended at each length (`ended[state][ℓ − 1]` of them) and that were seen
    /// to last at least each length and not to end (`going[state][ℓ − 1]`): each state with at
    /// least one ended. Up to the longest that ended, by Kaplan and Meier's product limit
    /// (a run going on is at risk of ending at each age it was seen to reach); past it, a
    /// geometric whose chance of going on is Laplace's rule over every symbol of the state seen,
    /// each a trial of going on or ending: (symbols − ends + 1) / (symbols + 2). With runs going
    /// on counted, it is never 0 or 1, so no run length is ever impossible: a state seen only
    /// in runs of one still has longer runs a chance.
    fn of(ended: &[Vec<usize>; 2], going: &[Vec<usize>; 2]) -> Runs {
        let fit = |x: usize| -> (Vec<f64>, f64) {
            let (e, g) = (&ended[x], &going[x]);
            let len = e.len().max(g.len());
            let at = |v: &Vec<usize>, l: usize| v.get(l).copied().unwrap_or(0);
            let mut risk: usize = e.iter().sum::<usize>() + g.iter().sum::<usize>();
            let mut left = 1.0;
            let mut survive = Vec::with_capacity(e.len());
            for l in 0..e.len() {
                survive.push(left);
                left *= 1.0 - at(e, l) as f64 / risk as f64;
                risk -= at(e, l) + at(g, l);
            }
            let exposure: usize = (0..len).map(|l| (l + 1) * (at(e, l) + at(g, l))).sum();
            let events: usize = e.iter().sum();
            let on = (exposure - events + 1) as f64 / (exposure + 2) as f64;
            (survive, on)
        };
        let ((a, on_a), (e, on_e)) = (fit(0), fit(1));
        Runs {
            survive: [a, e],
            on: [on_a, on_e],
        }
    }

    /// The chance a symbol of `state` is in a run at the age `survive[i]` stands for (the
    /// last, every age from L on), times the mean run of the state.
    fn weight(&self, state: Arrival, i: usize) -> f64 {
        let s = &self.survive[state as usize];
        match i + 1 == s.len() {
            true => s[i] / (1.0 - self.on[state as usize]),
            false => s[i],
        }
    }

    /// The mean run of `state`, in symbols.
    fn mean(&self, state: Arrival) -> f64 {
        (0..self.survive[state as usize].len())
            .map(|i| self.weight(state, i))
            .sum()
    }

    /// The share of symbols erased in the long run.
    pub fn loss(&self) -> f64 {
        let (a, e) = (self.mean(Arrival::Arrived), self.mean(Arrival::Erased));
        e / (a + e)
    }

    /// The chance a run of `state` that has lasted `age` symbols lasts one more.
    fn goes_on(&self, state: Arrival, age: usize) -> f64 {
        let s = &self.survive[state as usize];
        match s.get(age) {
            Some(&next) => next / s[age - 1],
            None => self.on[state as usize],
        }
    }

    /// The chance a run of `state` lasts at least `l` symbols.
    fn least(&self, state: Arrival, l: usize) -> f64 {
        let s = &self.survive[state as usize];
        match l <= s.len() {
            true => s[l - 1],
            false => s[s.len() - 1] * self.on[state as usize].powi((l - s.len()) as i32),
        }
    }
}

/// The repairs a block is sent with, and the chance it misses with them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Choice {
    pub m: usize,
    pub miss: f64,
}

/// One fit's miss chances: for n symbols in a row from the runs' long-run state, the chance
/// that more than m of them are erased, for every n asked of it so far.
///
/// Exactly, by renewal: the first symbol falls in a run of a state X with r symbols left of
/// it (r ≥ 1) with chance Σ_{j ≥ r} P(a run of X lasts at least j) / (both states' mean runs
/// summed), a closed form; every run after it starts fresh in the block, so a forward recursion
/// over state, age and count carries only ages less than n (and at most the longest run
/// remembered, L, past which a run goes on memoryless). Reaching n from n − 1 costs, for each
/// state, 2 × min(n − 1, L) × n multiply-adds ([`Table::cost`]), so a plan costs a lookup and the
/// steps no block of the fit took before, bounded by the block and the model's memory alone,
/// never by the session's length.
///
/// Each chance is summed from the counts above m, never taken as one less the rest, so a
/// chance of 1e-12 keeps its digits.
pub struct Table {
    runs: Runs,
    /// Per state, Σ_{j ≥ r} P(a run lasts at least j) for r = 1 to L, the last taking in
    /// every length past L; and both states' mean runs summed.
    beyond: [Vec<f64>; 2],
    total: f64,
    /// After the symbols reckoned: by state, age less one and count of erasures, the chance
    /// the last of them is in a run begun after the first symbol's.
    fresh: [Vec<Vec<f64>>; 2],
    /// `above[n − 1][m]`: the chance more than m of n are erased.
    above: Vec<Vec<f64>>,
    /// The most symbols a block asked of it.
    asked: usize,
    work: u64,
}

impl Table {
    pub fn new(runs: Runs) -> Table {
        let beyond = |x: Arrival| -> Vec<f64> {
            let s = &runs.survive[x as usize];
            let mut b = vec![0.0; s.len()];
            for i in (0..s.len()).rev() {
                b[i] = runs.weight(x, i) + b.get(i + 1).copied().unwrap_or(0.0);
            }
            b
        };
        let total = runs.mean(Arrival::Arrived) + runs.mean(Arrival::Erased);
        Table {
            beyond: [beyond(Arrival::Arrived), beyond(Arrival::Erased)],
            total,
            runs,
            fresh: [Vec::new(), Vec::new()],
            above: Vec::new(),
            asked: 0,
            work: 0,
        }
    }

    /// The runs it is of.
    pub fn runs(&self) -> &Runs {
        &self.runs
    }

    /// The chance the first symbol is in a run of `x` with at least `r` symbols left of it,
    /// itself included.
    fn going(&self, x: Arrival, r: usize) -> f64 {
        let b = &self.beyond[x as usize];
        let sum = match r <= b.len() {
            true => b[r - 1],
            false => {
                let on = self.runs.on[x as usize];
                self.runs.least(x, r) / (1.0 - on)
            }
        };
        sum / self.total
    }

    /// One symbol more: from the t reckoned to t + 1.
    fn step(&mut self) {
        let t = self.above.len();
        let width = t + 2;
        let states = [Arrival::Arrived, Arrival::Erased];
        let mut next: [Vec<Vec<f64>>; 2] = [Vec::new(), Vec::new()];
        for x in states {
            let ages = (t + 1).min(self.runs.survive[x as usize].len());
            next[x as usize] = vec![vec![0.0; width]; ages.max(1)];
        }
        let erased = |x: Arrival| (x == Arrival::Erased) as usize;
        let other = |x: Arrival| match x {
            Arrival::Arrived => Arrival::Erased,
            Arrival::Erased => Arrival::Arrived,
        };
        if t > 0 {
            for x in states {
                let ages = self.fresh[x as usize].len();
                for age in 1..=ages {
                    let on = self.runs.goes_on(x, age);
                    let older = (age + 1).min(next[x as usize].len());
                    for e in 0..=t {
                        let w = self.fresh[x as usize][age - 1][e];
                        if w == 0.0 {
                            continue;
                        }
                        next[x as usize][older - 1][e + erased(x)] += w * on;
                        let y = other(x);
                        next[y as usize][0][e + erased(y)] += w * (1.0 - on);
                    }
                }
                self.work += 2 * (ages * (t + 1)) as u64;
                // The first symbol's run ends at the t-th: the next starts fresh.
                let ended = self.runs.least(x, t) / self.total;
                let y = other(x);
                next[y as usize][0][t * erased(x) + erased(y)] += ended;
            }
        }
        self.fresh = next;
        // The counts after t + 1: the fresh runs', and the first run's if it goes on.
        let mut counts = vec![0.0; width];
        for x in states {
            for row in &self.fresh[x as usize] {
                for (c, &w) in counts.iter_mut().zip(row) {
                    *c += w;
                }
            }
            counts[(t + 1) * erased(x)] += self.going(x, t + 1);
        }
        let mut above = vec![0.0; t + 1];
        let mut sum = 0.0;
        for m in (0..=t).rev() {
            sum += counts[m + 1];
            above[m] = sum;
        }
        self.above.push(above);
    }

    /// Reckons to `n` symbols, if it has not.
    pub fn prepare(&mut self, n: usize) {
        while self.above.len() < n {
            self.step();
        }
    }

    /// The chance a block of `k` sources sent with `m` repairs misses: more than m of its
    /// k + m symbols erased.
    pub fn miss(&mut self, k: NonZeroUsize, m: usize) -> f64 {
        let n = k.get() + m;
        self.asked = self.asked.max(n);
        while self.above.len() < n {
            self.step();
        }
        self.above[n - 1][m]
    }

    /// The least m, up to `most`, whose miss chance for a block of `k` sources is at most
    /// `target`; or `most`, if none is.
    pub fn repairs(&mut self, k: NonZeroUsize, target: f64, most: usize) -> Choice {
        let mut choice = Choice { m: 0, miss: 1.0 };
        for m in 0..=most {
            choice = Choice {
                m,
                miss: self.miss(k, m),
            };
            if choice.miss <= target {
                break;
            }
        }
        choice
    }

    /// Multiply-adds spent so far.
    pub fn work(&self) -> u64 {
        self.work
    }

    /// What reaching n symbols costs from none: Σ_{t < n} Σ_X 2 × min(t, L_X) × (t + 1)
    /// multiply-adds, L_X the longest run of state X the fit remembers (at most
    /// [`Erasures::MEMORY`]).
    pub fn cost(&self, n: usize) -> u64 {
        let longest = |x: usize| self.runs.survive[x].len();
        (1..n)
            .map(|t| {
                (0..2)
                    .map(|x| 2 * (t.min(longest(x)) * (t + 1)) as u64)
                    .sum::<u64>()
            })
            .sum()
    }
}

/// A block's sizing by both fits ([`Erasures::fits`]): the larger repairs either asks for, and
/// the larger miss chance either gives with them. Two lookups a plan, each bounded as one
/// [`Table`]'s.
pub struct Tables([Table; 2]);

impl Tables {
    pub fn new(fits: [Runs; 2]) -> Tables {
        Tables(fits.map(Table::new))
    }

    /// The larger of the two fits' least m (up to `most`) meeting `target`, and the larger
    /// miss chance with it.
    pub fn repairs(&mut self, k: NonZeroUsize, target: f64, most: usize) -> Choice {
        let m = self
            .0
            .iter_mut()
            .map(|t| t.repairs(k, target, most).m)
            .max()
            .unwrap_or(most);
        Choice {
            m,
            miss: self.miss(k, m),
        }
    }

    /// The larger of the two fits' chances that a block of `k` sources with `m` repairs
    /// misses.
    pub fn miss(&mut self, k: NonZeroUsize, m: usize) -> f64 {
        self.0.iter_mut().map(|t| t.miss(k, m)).fold(0.0, f64::max)
    }

    /// The most symbols a plan asked of either table: the largest block sized from them
    /// (not what [`Tables::prepare`] reckoned ahead).
    pub fn asked(&self) -> usize {
        self.0.iter().map(|t| t.asked).max().unwrap_or(0)
    }

    /// Both tables reckoned to `n` symbols ahead of any plan, so that a plan sizing no larger
    /// block only reads them.
    pub fn prepare(&mut self, n: usize) {
        for t in &mut self.0 {
            t.prepare(n);
        }
    }

    /// Multiply-adds spent so far, both tables'.
    pub fn work(&self) -> u64 {
        self.0.iter().map(Table::work).sum()
    }

    /// What reaching n symbols costs both tables from none ([`Table::cost`]).
    pub fn cost(&self, n: usize) -> u64 {
        self.0.iter().map(|t| t.cost(n)).sum()
    }
}

/// The reported outcomes the runs are fitted to: the last [`Erasures::MEMORY`] symbols' in
/// send order.
pub struct Erasures {
    ring: VecDeque<Arrival>,
}

impl Erasures {
    /// The outcomes remembered: what the runs are fitted to.
    ///
    /// kind: measured. A block's miss chance lives in the runs' tail (a 16-symbol burst for a
    ///   block of 3 sources and 15 repairs), which only the longest runs remembered inform. Fitted
    ///   to 2,500 outcomes of a two-state chain (9% lost, bursts of three), the chance of 16
    ///   erased in 18 spread from 1.7e-4 to 3.0e-3 (10th to 90th percentile of 200 fits) about
    ///   the true 4.6e-4, and a plan, sized to the fit when it ran low, missed more than the
    ///   model expected: 18 misses where 12.9 were expected over the datagram and stream sims.
    ///   At 25,000 the spread is 2.5e-4 to 1.0e-3 and the sims missed 9 where 12.6 were.
    /// unit: symbols, in send order.
    /// scope: the erasure fit alone; the path's other samples and the refits' cadence keep
    ///   [`super::path::MEMORY`]. A change of the path's loss is read whole after this many
    ///   symbols (10 s of a busy screen, 70 s of production's), a tenth of it at each refit.
    /// evidence: `lanes::sim::tests`' freeze properties, which assert the misses within the
    ///   Poisson bound of the model's expectation.
    /// revalidate: when the loss model or the workload's symbol rate changes.
    pub const MEMORY: usize = 25_000;

    pub fn new() -> Erasures {
        Erasures {
            ring: VecDeque::with_capacity(Erasures::MEMORY),
        }
    }

    /// Symbol after symbol, in the order they were sent: what became of each.
    pub fn record(&mut self, now: Arrival) {
        if self.ring.len() == Erasures::MEMORY {
            self.ring.pop_front();
        }
        self.ring.push_back(now);
    }

    /// The outcomes the short fit reads: the last refit's worth, the same count as the path's
    /// other samples, so that it reads a change of the path's loss whole within a refit.
    pub const SHORT: usize = super::path::MEMORY;

    /// The two fits a block is sized by, each exact and the block given the larger repairs of
    /// the two, so that the reader never under-estimates erasure; over-protecting costs only
    /// bandwidth. The short fit reads the last [`Erasures::SHORT`] outcomes: how often bursts
    /// start, and how long they last, as the path is now. The tailed fit takes the short fit's
    /// arrival runs and the erasure runs of all [`Erasures::MEMORY`] outcomes: how long bursts
    /// can last, read from enough of them to see the rare long ones. Both start their bursts
    /// as often as the path does now, so after a change of the path's loss, worse or better,
    /// both follow it within a refit.
    pub fn fits(&self) -> [Runs; 2] {
        let short = self.runs_of(Erasures::SHORT);
        let long = self.runs_of(Erasures::MEMORY);
        let tailed = Runs {
            survive: [short.survive[0].clone(), long.survive[1].clone()],
            on: [short.on[0], long.on[1]],
        };
        [short, tailed]
    }

    /// The runs of all the remembered outcomes.
    pub fn runs(&self) -> Runs {
        self.runs_of(Erasures::MEMORY)
    }

    /// The runs the last `last` remembered outcomes show: those that began and ended among
    /// them, and the last, going on (the first, which may have begun before, is not counted
    /// unless it is the last); and one run of one symbol of each state besides, as Laplace's
    /// rule of succession adds one of each outcome, so that no state is ever impossible and,
    /// with nothing seen, each is as likely.
    fn runs_of(&self, last: usize) -> Runs {
        let mut ended = [vec![1usize], vec![1usize]];
        let mut going = [Vec::new(), Vec::new()];
        let count = |v: &mut Vec<usize>, len: usize| {
            if v.len() < len {
                v.resize(len, 0);
            }
            v[len - 1] += 1;
        };
        let mut run: Option<(Arrival, usize)> = None;
        let mut first = true;
        for &a in self.ring.iter().skip(self.ring.len().saturating_sub(last)) {
            run = match run {
                Some((state, len)) if state == a => Some((state, len + 1)),
                Some((state, len)) => {
                    if !first {
                        count(&mut ended[state as usize], len);
                    }
                    first = false;
                    Some((a, 1))
                }
                None => Some((a, 1)),
            };
        }
        if let Some((state, len)) = run {
            count(&mut going[state as usize], len);
        }
        Runs::of(&ended, &going)
    }
}

impl Default for Erasures {
    fn default() -> Erasures {
        Erasures::new()
    }
}

#[cfg(test)]
impl Runs {
    /// The runs whose lengths are counted in `lengths`, as [`Erasures::runs`] counts them.
    pub(crate) fn counted(lengths: [Vec<usize>; 2]) -> Runs {
        Runs::of(&lengths, &[Vec::new(), Vec::new()])
    }

    /// Runs with geometric lengths, as a two-state chain erases: after an arrival the next
    /// symbol is erased with chance `a`, after an erasure with chance `b`.
    pub(crate) fn geometric(a: f64, b: f64) -> Runs {
        Runs {
            survive: [vec![1.0], vec![1.0]],
            on: [1.0 - a, b],
        }
    }

    /// Outcomes drawn from the runs, from their long-run state.
    pub(crate) fn draw<'a>(
        &'a self,
        rng: &'a mut crate::xorshift::Xorshift,
    ) -> impl Iterator<Item = Arrival> + 'a {
        let mut uniform = move || rng.below(1 << 53) as f64 / (1u64 << 53) as f64;
        let total = self.mean(Arrival::Arrived) + self.mean(Arrival::Erased);
        let mut pick = uniform() * total;
        let mut now = (Arrival::Erased, self.survive[1].len());
        'start: for state in [Arrival::Arrived, Arrival::Erased] {
            for i in 0..self.survive[state as usize].len() {
                let w = self.weight(state, i);
                if pick < w {
                    now = (state, i + 1);
                    break 'start;
                }
                pick -= w;
            }
        }
        let mut first = true;
        std::iter::from_fn(move || {
            if !first {
                let (state, age) = now;
                now = match uniform() < self.goes_on(state, age) {
                    true => (state, (age + 1).min(self.survive[state as usize].len())),
                    false => match state {
                        Arrival::Arrived => (Arrival::Erased, 1),
                        Arrival::Erased => (Arrival::Arrived, 1),
                    },
                };
            }
            first = false;
            Some(now.0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xorshift::Xorshift;

    /// Run-length counts drawn at random: up to six lengths of each state, counts up to nine.
    fn counts(rng: &mut Xorshift) -> [Vec<usize>; 2] {
        let mut one = || {
            let mut c: Vec<usize> = (0..rng.range(1, 7))
                .map(|_| rng.below(10) as usize)
                .collect();
            *c.last_mut().unwrap() += 1;
            c
        };
        [one(), one()]
    }

    /// The chance of one pattern of outcomes under the runs counted in `lengths`, from their
    /// long-run state, read off its runs directly: the first may be met at any age, each one
    /// between ends at exactly its length, the last lasts at least its length. A run lasts at
    /// least l with the share counted so, and past the longest counted, L, with that share
    /// times on^(l − L), on = (symbols − runs + 1) / (symbols + 2) of the state counted; the
    /// sums over ages run until the terms are below 1e-30.
    fn chance(lengths: &[Vec<usize>; 2], pattern: &[Arrival]) -> f64 {
        let counted = |x: Arrival, l: usize| -> f64 {
            let c = &lengths[x as usize];
            let total: usize = c.iter().sum();
            c.iter().skip(l - 1).sum::<usize>() as f64 / total as f64
        };
        let longest = |x: Arrival| lengths[x as usize].len();
        let on = |x: Arrival| {
            let c = &lengths[x as usize];
            let symbols: usize = c.iter().enumerate().map(|(l, n)| (l + 1) * n).sum();
            let ends: usize = c.iter().sum();
            (symbols - ends + 1) as f64 / (symbols + 2) as f64
        };
        let least = |x: Arrival, l: usize| -> f64 {
            match l <= longest(x) {
                true => counted(x, l),
                false => counted(x, longest(x)) * on(x).powi((l - longest(x)) as i32),
            }
        };
        let at = |x: Arrival, l: usize| least(x, l) - least(x, l + 1);
        let ages = |x: Arrival| {
            let q = on(x);
            match q > 0.0 {
                true => longest(x) + pattern.len() + (-30.0 / q.log10()) as usize,
                false => longest(x) + pattern.len(),
            }
        };
        let mean = |x: Arrival| -> f64 { (1..=ages(x)).map(|l| least(x, l)).sum() };
        let total = mean(Arrival::Arrived) + mean(Arrival::Erased);
        let mut runs: Vec<(Arrival, usize)> = Vec::new();
        for &a in pattern {
            match runs.last_mut() {
                Some((x, l)) if *x == a => *l += 1,
                _ => runs.push((a, 1)),
            }
        }
        let (x, l) = runs[0];
        let mut p = match runs.len() {
            1 => (1..=ages(x)).map(|age| least(x, age + l - 1)).sum::<f64>() / total,
            _ => (1..=ages(x)).map(|age| at(x, age + l - 1)).sum::<f64>() / total,
        };
        if runs.len() > 1 {
            for &(x, l) in &runs[1..runs.len() - 1] {
                p *= at(x, l);
            }
            let &(x, l) = runs.last().unwrap();
            p *= least(x, l);
        }
        p
    }

    /// For n up to 12, every m under it, and runs drawn at random, the recursion's chance equals
    /// the sum over all 2^n erasure patterns with more than m erased of each pattern's chance.
    #[test]
    fn the_recursion_is_the_enumeration() {
        let mut rng = Xorshift::new(0x9E37_79B9_7F4A_7C15);
        for _ in 0..60 {
            let lengths = counts(&mut rng);
            let mut table = Table::new(Runs::counted(lengths.clone()));
            let n = rng.range(1, 13) as usize;
            let mut patterns = vec![0.0; n + 1];
            for bits in 0u32..1 << n {
                let pattern: Vec<Arrival> = (0..n)
                    .map(|i| match bits >> i & 1 {
                        1 => Arrival::Erased,
                        _ => Arrival::Arrived,
                    })
                    .collect();
                patterns[bits.count_ones() as usize] += chance(&lengths, &pattern);
            }
            let whole: f64 = patterns.iter().sum();
            assert!((whole - 1.0).abs() < 1e-12, "{lengths:?} n {n}: {whole}");
            for m in 0..n {
                let sum: f64 = patterns[m + 1..].iter().sum();
                let k = NonZeroUsize::new(n - m).unwrap();
                let r = table.miss(k, m);
                assert!(
                    (r - sum).abs() <= 1e-12 * sum + 1e-15,
                    "{lengths:?} n {n} m {m}: recursion {r}, enumeration {sum}"
                );
            }
        }
    }

    /// Fitted to outcomes drawn from runs, the fit's share of runs lasting each length it saw
    /// is within four of its standard errors of the runs', for runs from memoryless to long
    /// bursts.
    #[test]
    fn the_fit_finds_the_runs() {
        let mut rng = Xorshift::new(0xD1B5_4A32_D192_ED03);
        for _ in 0..12 {
            let lengths = counts(&mut rng);
            let runs = Runs::counted(lengths.clone());
            let mut fit = Erasures::new();
            let mut drawn = Xorshift::new(rng.below(u64::MAX));
            for a in runs.draw(&mut drawn).take(Erasures::MEMORY) {
                fit.record(a);
            }
            let found = fit.runs();
            let n = Erasures::MEMORY as f64
                / (runs.mean(Arrival::Arrived) + runs.mean(Arrival::Erased));
            for x in [Arrival::Arrived, Arrival::Erased] {
                for l in 1..=found.survive[x as usize].len() {
                    let (w, g) = (runs.least(x, l), found.least(x, l));
                    let se = (w * (1.0 - w) / n).sqrt().max(1.0 / n);
                    assert!(
                        (w - g).abs() < 4.0 * se,
                        "{lengths:?} {x:?} length {l}: runs {w}, fit {g}"
                    );
                }
            }
        }
    }

    /// A run seen going on counts: with nothing but arrivals remembered, the arrivals' mean run
    /// is Laplace's over every one of them and the Laplace run, the erasures' over the Laplace
    /// run alone, and the share erased follows from the two.
    #[test]
    fn a_run_going_on_counts() {
        let mut fit = Erasures::new();
        for _ in 0..Erasures::MEMORY {
            fit.record(Arrival::Arrived);
        }
        let loss = fit.runs().loss();
        // Laplace's rule over each state's symbols: a mean run of (symbols + 2) / (ends + 1);
        // the arrivals' one Laplace run ended and MEMORY going on, the erasures' Laplace run.
        let arrivals = (Erasures::MEMORY as f64 + 1.0 + 2.0) / 2.0;
        let erasures = (1.0 + 2.0) / 2.0;
        let want = erasures / (arrivals + erasures);
        assert!((loss - want).abs() < 1e-12 * want, "{loss} not {want}");
    }

    /// Runs of geometric lengths are the two-state chain: for chains drawn at random and n up
    /// to 12, the table's chance that more than m of n are erased equals the sum over all 2^n
    /// patterns of each pattern's chance under the chain from its long-run state.
    #[test]
    fn geometric_runs_are_the_chain() {
        let mut rng = Xorshift::new(0xBB67_AE85_84CA_A73B);
        for _ in 0..40 {
            let a = rng.range(1, 1000) as f64 / 1000.0;
            let b = rng.range(0, 1000) as f64 / 1000.0;
            let mut table = Table::new(Runs::geometric(a, b));
            let n = rng.range(1, 13) as usize;
            let mut above = vec![0.0; n + 1];
            for bits in 0u32..1 << n {
                let mut chance = 1.0;
                let mut last = None;
                for i in 0..n {
                    let erased = bits >> i & 1 == 1;
                    let pe = match last {
                        None => a / (a + 1.0 - b),
                        Some(false) => a,
                        Some(true) => b,
                    };
                    chance *= if erased { pe } else { 1.0 - pe };
                    last = Some(erased);
                }
                above[bits.count_ones() as usize] += chance;
            }
            for m in 0..n {
                let want: f64 = above[m + 1..].iter().sum();
                let got = table.miss(NonZeroUsize::new(n - m).unwrap(), m);
                assert!(
                    (got - want).abs() <= 1e-12 * want + 1e-15,
                    "a {a} b {b} n {n} m {m}: table {got}, chain {want}"
                );
            }
        }
    }
}
