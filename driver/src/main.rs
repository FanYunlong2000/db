use driver::Eval;

fn main() {
  Eval::default().exec_repl(include_str!("../../tests/sql/test_select.sql"));
}