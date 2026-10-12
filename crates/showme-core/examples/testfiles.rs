//! Writes a few PDFs for trying ShowMe out: `cargo run --example testfiles -- <folder>`.

use std::path::PathBuf;

fn main() {
    let folder = PathBuf::from(std::env::args().nth(1).expect("which folder?"));
    let pets = "Pets: the tenant may keep one cat in the flat. Dogs, birds and reptiles are not allowed.";
    let rent = "Rent: 12,500 kroner a month, paid on the first of each month to the landlord's account.";
    showme_core::pdf::write_test_pdf(
        &folder.join("lease.pdf"),
        &[vec!["Lease agreement for Storgata 14, Oslo", rent, rent], vec!["Rules of the house", pets, pets, pets]],
    );
    let plan = "Flight to Tokyo Haneda on November 3. Then the train to Kamakura to see the Great Buddha.";
    showme_core::pdf::write_test_pdf(&folder.join("itinerary.pdf"), &[vec!["Japan trip", plan, plan, plan]]);
    showme_core::pdf::write_test_pdf(
        &folder.join("error.pdf"),
        &[vec!["Payment declined", "Your card was declined. Error code K7Q2LM: the card has expired."]],
    );
}
