# XMrsPlayer is a safe no_std soundtracker music player

XMrsPlayer is a crate to play real music

This crate player does not aim to be 100% compatible with all music since it is impossible due to the historical and changing nature of the demo scene, but it should be close enough to a good result to amaze you :-)

## File format

**Amiga Module**, **S3M** and **XM** player works.

The **S3M** format is a work in progress and works generally well. Help is welcome if you want identical effects. The `xmrs` structures are also designed to handle FM instruments.

The **SID** format was added from the reverse engineering of Rob Hubbard's 6510 player code when I had time. For now, I can extract the tracks and effects from some of his big hits using the generic player of the modules.

- Eventually, it may be necessary to work by *Track* to build the *Patterns* because the lengths are changing on some *Tracks* which makes it impossible to generate homogeneous patterns (but I can still do it for a large part of his music)
- Above all, add to the player the use of the `resid-rs` box, because any other means is really doomed to failure. As you can see, the code is generic enough to do what we want, I just lack time so if there are people who speak rust and who are motivated to have fun with me :-)

The **IT** format is a WIP, loader mostly done.

## Use it as a Crate

As per usual:

```
$ cargo add xmrsplayer
```

Then look about features in documentation! Remember that you can do both embedded and classic applications with this project.

`micromath` is used by default in `no_std`. If you prefer libm, use `cargo build --no-default-features --features=libm --release`.

If you want to use `std` feature use `cargo build --no-default-features --features=std --release`

# Install it as a CLI player

Directly from crate.io:

```
$ cargo install xmrsplayer --features=demo
```

From a local git directory:

```
$ cargo install --path . --features=demo
```

# Use it as a CLI player from git repo

```
$ cargo run --features=demo --release -- --help
```

## Some additional notes

This code and its dependency `xmrs` which is used for data structures _does not use_ any `no_safe` part.

Likewise, there is a real desire to limit dependencies through the use of features. This is not only important for embedded!

It should be possible to gradually downgrade the code to older processors by successive optimizations and it would be very fun to see what it will give with LLVM compilation optimizations on architectures which don't even have floats like the [Z80](https://github.com/jacobly0/llvm-project), [MOS 6502 and related processor](https://github.com/llvm-mos/llvm-mos) or more simply on the 68k target.

Finally, it was written with the intention of extension and sharing, feel free to use it for your own projects. I will be very happy to be informed about it via an issue on codeberg for example, but it is not mandatory.

