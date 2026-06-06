#include "lld/Common/Driver.h"

#include <cstdlib>
#include <optional>

LLD_HAS_DRIVER(coff)
LLD_HAS_DRIVER(elf)
LLD_HAS_DRIVER(mingw)
LLD_HAS_DRIVER(macho)
LLD_HAS_DRIVER(wasm)

std::vector<const char *> parseArgs(std::string cmd) {
    std::string curArg = "";
    std::vector<const char*> args;

    for(char c : cmd) {
        if(c == ' ') {
            // Deep copy the arg
            const std::string::size_type size = curArg.size()+1;
            char *arg = new char[size];
            memcpy(arg, curArg.c_str(), size);
            
            args.push_back((const char*)arg);
            curArg.clear();
        } else {
            curArg.push_back(c);
        }
    }

    // Deep copy the arg
    const std::string::size_type size = curArg.size()+1;
    char *arg = new char[size];
    memcpy(arg, curArg.c_str(), size);
    
    args.push_back((const char*)arg);
    curArg.clear();

    return args;
}

extern "C" {
  int lldEntry(const char* cmd) {
    return lld::lldMain(parseArgs(std::string(cmd)), llvm::outs(), llvm::errs(), LLD_ALL_DRIVERS).retCode;
  }
}