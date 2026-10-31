// A cell's value borrowed with the thread's token shared, then written
// with it exclusive: the borrow and the write would overlap.
use elise::{LocalToken, TokenCell};

fn main() {
    let mut token = LocalToken::new();
    let cell = TokenCell::new(String::from("style"));
    let read = cell.get(&token);
    cell.replace(&mut token, String::new());
    println!("{read}");
}
