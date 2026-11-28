// A long-lived DXC for the launcher. It runs the compiler's own stages with one
// optimisation pass left out, which the dxc command line has no switch for.
//
// A request is four lines: the HLSL path, the output path, the target profile
// and the remaining dxc options, separated by spaces. The answer is "ok" once
// the DXIL is written, or "no" when this compiler cannot build the program.
#include <dxc/dxcapi.h>

#include <fstream>
#include <iostream>
#include <iterator>
#include <map>
#include <sstream>
#include <stdexcept>
#include <string>
#include <vector>

// Correlated value propagation takes about a quarter of DXC's time on
// translated console programs and changes next to nothing in them.
static const char *const SKIPPED_PASS = "-correlated-propagation";

static void check(HRESULT status, const char *operation) {
    if (FAILED(status)) throw std::runtime_error(operation);
}

static std::wstring widen(const std::string &text) { return std::wstring(text.begin(), text.end()); }

// The lane count a program's [WaveSize(N)] asks for, or 0 without one.
static unsigned wave_size(const std::string &source) {
    const std::string attribute = "[WaveSize(";
    const auto at = source.find(attribute);
    if (at == std::string::npos) return 0;
    std::size_t end = at + attribute.size();
    unsigned lanes = 0;
    while (end < source.size() && source[end] >= '0' && source[end] <= '9' && lanes < 1000)
        lanes = lanes * 10 + (source[end++] - '0');
    // A range, or a second entry point, is left to the command line.
    if (!lanes || source.compare(end, 2, ")]") != 0 || source.find(attribute, end) != std::string::npos)
        throw std::runtime_error("unsupported WaveSize attribute");
    return lanes;
}

// The metadata line `!name = !{...}`, without its closing brace.
static std::size_t node_end(const std::string &text, const std::string &name) {
    const auto at = text.find("\n" + name + " = !{");
    const auto end = at == std::string::npos ? at : text.find('\n', at + 1);
    if (end == std::string::npos || text[end - 1] != '}') throw std::runtime_error("unexpected metadata");
    return end - 1;
}

// The last `!N` of a metadata line.
static std::string last_reference(const std::string &text, std::size_t end) {
    const auto at = text.rfind('!', end);
    std::size_t stop = at + 1;
    while (stop < text.size() && text[stop] >= '0' && text[stop] <= '9') ++stop;
    if (at == std::string::npos || stop == at + 1 || stop != end) throw std::runtime_error("unexpected metadata");
    return text.substr(at, stop - at);
}

struct Dxc {
    CComPtr<IDxcCompiler3> compiler;
    CComPtr<IDxcOptimizer> optimizer;
    CComPtr<IDxcAssembler> assembler;
    CComPtr<IDxcUtils> utils;
    // The pass list depends only on the profile and options.
    std::map<std::string, std::vector<std::wstring>> pipelines;

    Dxc() {
        check(DxcCreateInstance(CLSID_DxcCompiler, IID_PPV_ARGS(&compiler)), "create compiler");
        check(DxcCreateInstance(CLSID_DxcOptimizer, IID_PPV_ARGS(&optimizer)), "create optimizer");
        check(DxcCreateInstance(CLSID_DxcAssembler, IID_PPV_ARGS(&assembler)), "create assembler");
        check(DxcCreateInstance(CLSID_DxcUtils, IID_PPV_ARGS(&utils)), "create utilities");
    }

    CComPtr<IDxcBlob> front_end(const std::string &source, const std::vector<std::wstring> &options,
                                const wchar_t *stop) {
        std::vector<LPCWSTR> arguments;
        for (const auto &option : options) arguments.push_back(option.c_str());
        arguments.push_back(stop);
        const DxcBuffer buffer{source.data(), source.size(), DXC_CP_UTF8};
        CComPtr<IDxcResult> result;
        check(compiler->Compile(&buffer, arguments.data(), static_cast<UINT32>(arguments.size()), nullptr,
                                IID_PPV_ARGS(&result)), "compile call");
        HRESULT status{};
        check(result->GetStatus(&status), "read compile status");
        check(status, "compile failed");
        CComPtr<IDxcBlob> output;
        check(result->GetResult(&output), "get compiler output");
        if (!output || !output->GetBufferSize()) throw std::runtime_error("empty compiler output");
        return output;
    }

    const std::vector<std::wstring> &passes(const std::string &key, const std::string &source,
                                            const std::vector<std::wstring> &options) {
        const auto found = pipelines.find(key);
        if (found != pipelines.end()) return found->second;
        const auto dump = front_end(source, options, L"-Odump");
        std::istringstream lines(std::string(static_cast<const char *>(dump->GetBufferPointer()),
                                             dump->GetBufferSize()));
        std::vector<std::wstring> list;
        std::size_t skipped = 0;
        for (std::string line; std::getline(lines, line);) {
            while (!line.empty() && (line.back() == '\r' || line.back() == '\0')) line.pop_back();
            if (line.empty() || line[0] != '-') continue;
            if (line == SKIPPED_PASS) ++skipped;
            else list.push_back(widen(line));
        }
        // Another DXC release may name its passes differently. The command line
        // compiles the program then.
        if (list.empty() || !skipped) throw std::runtime_error("unexpected pass list");
        return pipelines.emplace(key, std::move(list)).first->second;
    }

    // The first stage's module does not carry the entry point's wave size to the
    // passes, so the DXIL would run at whatever width the GPU picks. Write it
    // back as dxc records it: property 11 of the entry point.
    CComPtr<IDxcBlob> with_wave_size(IDxcBlob *module, unsigned lanes) {
        const DxcBuffer buffer{module->GetBufferPointer(), module->GetBufferSize(), 0};
        CComPtr<IDxcResult> result;
        check(compiler->Disassemble(&buffer, IID_PPV_ARGS(&result)), "disassemble call");
        CComPtr<IDxcBlobUtf8> listing;
        check(result->GetOutput(DXC_OUT_DISASSEMBLY, IID_PPV_ARGS(&listing), nullptr), "get listing");
        std::string text(listing->GetStringPointer(), listing->GetStringLength());
        while (!text.empty() && text.back() == '\0') text.pop_back();
        if (text.empty() || text.back() != '\n') text += '\n';
        const std::string entries = "\n!dx.entryPoints = !{";
        const auto at = text.find(entries);
        if (at == std::string::npos) throw std::runtime_error("no entry point metadata");
        const auto entry = last_reference(text, text.find('}', at + entries.size()));
        const auto properties = last_reference(text, node_end(text, entry));
        const auto end = node_end(text, properties);
        text.insert(end, ", i32 11, !999999999");
        text += "!999999999 = !{i32 " + std::to_string(lanes) + "}\n";
        CComPtr<IDxcBlobEncoding> edited;
        check(utils->CreateBlob(text.data(), static_cast<UINT32>(text.size()), DXC_CP_UTF8, &edited), "copy listing");
        CComPtr<IDxcOperationResult> assembled;
        check(assembler->AssembleToContainer(edited, &assembled), "reassemble call");
        HRESULT status{};
        check(assembled->GetStatus(&status), "read reassembly status");
        check(status, "reassembly failed");
        CComPtr<IDxcBlob> container;
        check(assembled->GetResult(&container), "get reassembled DXIL");
        return container;
    }

    void compile(const std::string &source_path, const std::string &output_path, const std::string &target,
                 const std::string &flags) {
        std::ifstream input(source_path, std::ios::binary);
        if (!input) throw std::runtime_error("open HLSL input");
        const std::string source{std::istreambuf_iterator<char>{input}, {}};
        std::vector<std::wstring> options{widen(source_path), L"-T", widen(target)};
        if (target.rfind("lib_", 0) != 0) options.insert(options.end(), {L"-E", L"main"});
        std::istringstream words(flags);
        for (std::string word; words >> word;) options.push_back(widen(word));

        const auto &list = passes(target + " " + flags, source, options);
        std::vector<LPCWSTR> pointers;
        for (const auto &pass : list) pointers.push_back(pass.c_str());
        const auto module = front_end(source, options, L"-fcgl");
        CComPtr<IDxcBlob> optimised;
        check(optimizer->RunOptimizer(module, pointers.data(), static_cast<UINT32>(pointers.size()), &optimised,
                                      nullptr), "optimise");
        CComPtr<IDxcBlob> bytecode;
        if (const unsigned lanes = wave_size(source)) {
            bytecode = with_wave_size(optimised, lanes);
        } else {
            CComPtr<IDxcOperationResult> assembled;
            check(assembler->AssembleToContainer(optimised, &assembled), "assemble call");
            HRESULT status{};
            check(assembled->GetStatus(&status), "read assembly status");
            check(status, "assembly failed");
            check(assembled->GetResult(&bytecode), "get DXIL");
        }
        std::ofstream output(output_path, std::ios::binary);
        output.write(static_cast<const char *>(bytecode->GetBufferPointer()), bytecode->GetBufferSize());
        output.close();
        if (!output) throw std::runtime_error("write DXIL");
    }
};

int main() {
    try {
        Dxc dxc;
        std::string source, output, target, flags;
        while (std::getline(std::cin, source)) {
            if (!std::getline(std::cin, output) || !std::getline(std::cin, target) || !std::getline(std::cin, flags))
                throw std::runtime_error("incomplete compiler request");
            try {
                dxc.compile(source, output, target, flags);
                std::cout << "ok\n" << std::flush;
            } catch (const std::runtime_error &error) {
                std::cerr << error.what() << '\n';
                std::cout << "no\n" << std::flush;
            }
        }
        if (!std::cin.eof()) throw std::runtime_error("read compiler request");
    } catch (const std::exception &error) {
        std::cerr << error.what() << '\n';
        return 1;
    }
}
