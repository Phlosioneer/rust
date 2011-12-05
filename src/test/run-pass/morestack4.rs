// xfail-test
// compile-flags:--stack-growth

// This is testing for stack frames greater than 256 bytes,
// for which function prologues are generated differently

type biggy = {
    a00: u64,
    a01: u64,
    a02: u64,
    a03: u64,
    a04: u64,
    a05: u64,
    a06: u64,
    a07: u64,
    a08: u64,
    a09: u64,
    a10: u64,
    a11: u64,
    a12: u64,
    a13: u64,
    a14: u64,
    a15: u64,
    a16: u64,
    a17: u64,
    a18: u64,
    a19: u64,
    a20: u64,
    a21: u64,
    a22: u64,
    a23: u64,
    a24: u64,
    a25: u64,
    a26: u64,
    a27: u64,
    a28: u64,
    a29: u64,
    a30: u64,
    a31: u64,
    a32: u64,
    a33: u64,
    a34: u64,
    a35: u64,
    a36: u64,
    a37: u64,
    a38: u64,
    a39: u64,
};


fn getbig(i: biggy) {
    if i.a00 != 0u64 {
        getbig({a00: i.a00 - 1u64 with i});
    }
}

fn main() {
    getbig({
        a00: 100000u64,
        a01: 100000u64,
        a02: 100000u64,
        a03: 100000u64,
        a04: 100000u64,
        a05: 100000u64,
        a06: 100000u64,
        a07: 100000u64,
        a08: 100000u64,
        a09: 100000u64,
        a10: 100000u64,
        a11: 100000u64,
        a12: 100000u64,
        a13: 100000u64,
        a14: 100000u64,
        a15: 100000u64,
        a16: 100000u64,
        a17: 100000u64,
        a18: 100000u64,
        a19: 100000u64,
        a20: 100000u64,
        a21: 100000u64,
        a22: 100000u64,
        a23: 100000u64,
        a24: 100000u64,
        a25: 100000u64,
        a26: 100000u64,
        a27: 100000u64,
        a28: 100000u64,
        a29: 100000u64,
        a30: 100000u64,
        a31: 100000u64,
        a32: 100000u64,
        a33: 100000u64,
        a34: 100000u64,
        a35: 100000u64,
        a36: 100000u64,
        a37: 100000u64,
        a38: 100000u64,
        a39: 100000u64,
    });
}