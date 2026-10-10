//! 実績（actual_facts）と、その取込・割当・締めの API。
//!
//! 第4段階で API を移す。今はシナリオの切り替えが使う「実績の修正あり」の判定（drift）だけを持つ。

pub mod drift;

pub use drift::drifted_scenarios;
