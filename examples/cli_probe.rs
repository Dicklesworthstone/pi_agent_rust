use clap::Parser;
fn main() {
    let mut cmd = pi::cli::Cli::command();
    cmd.build();
    for a in cmd.get_arguments() {
        if let Some(long) = a.get_long() {
            println!("{long}");
        }
    }
}
