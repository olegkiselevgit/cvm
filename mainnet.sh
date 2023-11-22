#!/bin/sh
RUST_BACKTRACE=1 RUST_TRACE=trace cargo run --bin mantis -- --rpc-centauri "https://composable-rpc.polkachu.com:443" --grpc-centauri "https://composable-grpc.polkachu.com:22290" --osmosis "todo" --neutron "todo" --cvm-contract "centauri1wpf2szs4uazej8pe7g8vlck34u24cvxx7ys0esfq6tuw8yxygzuqpjsn0d" --wallet "$WALLET" --order-contract "centauri18ag4r8s6qug4gxwllzw04nj3ht6unk9g93e9sj5lscsukfkf4j6qxe7wsx" --simulate "200000ppica,3ibc/EF48E6B1A1A19F47ECAEA62F5670C37C0580E86A9E88498B7E393EB6F49F33C0"   