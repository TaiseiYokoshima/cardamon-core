build:
   cargo build --release


bare: build
   target/release/cardamon run bare



docker: build
   target/release/cardamon run docker
