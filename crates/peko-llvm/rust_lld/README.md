LLD is the llvm linker that allows PekoScript to cross link to multiple platforms.
This maps the LLD Api to linkable methods which are then implemented in src/lld.rs.

*To Compile* clang++ -c lldapi.cpp -I <PATH TO LLVM TOOLCHAIN>/include -fno-rtti -std=c++17
