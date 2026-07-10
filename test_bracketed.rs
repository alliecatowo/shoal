use reedline::Reedline;

fn main() {
    let _ = Reedline::create().with_bracketed_paste(true);
}
