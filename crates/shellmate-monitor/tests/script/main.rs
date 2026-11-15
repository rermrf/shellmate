//! The collector script itself, run under real shells (dash and the system's sh) against a pretend `/proc`
//! and stand-in commands: the frames it writes, the forks it makes, how it ends.

mod fakeroot;
mod run;
