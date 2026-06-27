#include "lld/Common/Driver.h"

#include <cstdlib>
#include <cstring>
#include <optional>
#include <string>
#include <vector>

LLD_HAS_DRIVER(coff)
LLD_HAS_DRIVER(elf)
LLD_HAS_DRIVER(mingw)
LLD_HAS_DRIVER(macho)
LLD_HAS_DRIVER(wasm)

// Split a command string into arguments. Arguments are separated by spaces,
// except inside double quotes, so a path containing spaces is passed as one
// argument when the caller wraps it in quotes. The quote characters are
// removed; the argument keeps only the text between and around them.
std::vector<const char *> parseArgs(std::string cmd) {
    std::vector<const char *> args;
    std::string curArg = "";
    bool inQuote = false;
    bool started = false;

    auto flush = [&]() {
        const std::string::size_type size = curArg.size() + 1;
        char *arg = new char[size];
        memcpy(arg, curArg.c_str(), size);
        args.push_back((const char *)arg);
        curArg.clear();
        started = false;
    };

    for (char c : cmd) {
        if (c == '"') {
            // A quote toggles quoting and starts an argument, so an empty
            // quoted string still produces an (empty) argument.
            inQuote = !inQuote;
            started = true;
        } else if (c == ' ' && !inQuote) {
            if (started) {
                flush();
            }
        } else {
            curArg.push_back(c);
            started = true;
        }
    }

    if (started) {
        flush();
    }

    return args;
}

extern "C" {
  int lldEntry(const char* cmd) {
    return lld::lldMain(parseArgs(std::string(cmd)), llvm::outs(), llvm::errs(), LLD_ALL_DRIVERS).retCode;
  }
}
