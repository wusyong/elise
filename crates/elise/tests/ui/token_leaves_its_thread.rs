// The token stays on its thread (another has its own), and a cell is
// reached on one thread only: else two tokens would reach one cell.
use elise::{LocalToken, TokenCell};

fn main() {
    let token = LocalToken::new();
    std::thread::spawn(move || drop(token));
    let cell = TokenCell::new(1);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let _ = &cell;
        });
    });
}
