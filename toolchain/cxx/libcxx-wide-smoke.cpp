//J946_r12_001
// SH9-c7: libc++ with wide characters, the way LLVM uses them (std::wstring through
// ConvertUTFWrapper, wide streams in a few tools). Built against the installed include/c++/v1 +
// lib layer only. It reaches relibc's new wcstof/wcstold, the fixed wcstod and wcsxfrm, and the
// wide _l calls behind the classic locale's wchar_t facets. — TEAL
#include <format>
#include <iostream>
#include <locale>
#include <regex>
#include <sstream>
#include <string>

static int fail(const char* why) {
    std::cout << "SH9-c7: " << why << "\nSH9-c7 wide libc++ FAILED" << std::endl;
    return 1;
}

extern "C" int main() {
    std::cout << "SH9-c7 wide libc++ begin" << std::endl;

    // 1. Wide strings and the numeric conversions over relibc's wcsto* family.
    {
        std::wstring text = L"kumo";
        text += L"-c7";
        if (text != L"kumo-c7" || text.size() != 7 || text.find(L"c7") != 5) return fail("wstring");
        if (std::to_wstring(42) != L"42" || std::to_wstring(-7) != L"-7") return fail("to_wstring");
        if (std::stoi(L"123") != 123 || std::stol(L"-45") != -45 || std::stoull(L"ff", nullptr, 16) != 255)
            return fail("stoi family");
        // wcstod used to stop at the exponent and read 3.25.
        if (std::stod(L"3.25e2") != 325.0 || std::stof(L"0.5") != 0.5f || std::stold(L"1.5") != 1.5L)
            return fail("stod family");
        std::size_t used = 0;
        if (std::stod(L"  -2.5x", &used) != -2.5 || used != 6) return fail("stod end position");
    }
    std::cout << "SH9-c7: wstring and wide numeric conversions -> OK" << std::endl;

    // 2. Wide streams, and std::wcout itself.
    {
        std::wostringstream out;
        out << 42 << L' ' << std::hex << 255 << std::dec << L' ' << 2.5;
        if (out.str() != L"42 ff 2.5") return fail("wostringstream");
        std::wistringstream in(L"7 3.5 word");
        int number = 0;
        double real = 0;
        std::wstring word;
        if (!(in >> number >> real >> word) || number != 7 || real != 3.5 || word != L"word")
            return fail("wistringstream");
        std::wcout << L"SH9-c7: wide streams and wcout -> OK" << std::endl;
        if (!std::wcout) return fail("wcout state");
    }

    // 3. The classic locale's wchar_t facets: ctype, collate (wcscoll_l/wcsxfrm_l) and codecvt.
    {
        const std::locale& classic = std::locale::classic();
        const auto& ctype = std::use_facet<std::ctype<wchar_t>>(classic);
        if (ctype.toupper(L'a') != L'A' || ctype.tolower(L'Q') != L'q' || !ctype.is(std::ctype_base::alpha, L'x') ||
            ctype.is(std::ctype_base::digit, L'x') || !ctype.is(std::ctype_base::space, L'\t') ||
            ctype.widen('k') != L'k' || ctype.narrow(L'k', '?') != 'k')
            return fail("ctype<wchar_t>");
        if (!std::isalpha(L'Z', classic) || std::isdigit(L'Z', classic)) return fail("isalpha(wchar_t, locale)");
        // The classic facets answer from libc++'s own tables; the _byname facets of a locale
        // named "C" go through relibc: isw*_l/tow*_l for ctype, wcscoll_l/wcsxfrm_l for collate.
        const std::locale named(std::locale(classic, new std::ctype_byname<wchar_t>("C")),
                                new std::collate_byname<wchar_t>("C"));
        const auto& named_ctype = std::use_facet<std::ctype<wchar_t>>(named);
        if (named_ctype.toupper(L'a') != L'A' || named_ctype.tolower(L'Q') != L'q' ||
            !named_ctype.is(std::ctype_base::alpha, L'x') || named_ctype.is(std::ctype_base::digit, L'x') ||
            !named_ctype.is(std::ctype_base::xdigit, L'f') || !named_ctype.is(std::ctype_base::punct, L'!') ||
            !named_ctype.is(std::ctype_base::space, L'\t') || !std::isupper(L'Q', named))
            return fail("ctype_byname<wchar_t>");
        const auto& collate = std::use_facet<std::collate<wchar_t>>(named);
        const std::wstring a = L"apple", b = L"banana";
        if (collate.compare(a.data(), a.data() + a.size(), b.data(), b.data() + b.size()) >= 0 ||
            collate.compare(b.data(), b.data() + b.size(), b.data(), b.data() + b.size()) != 0)
            return fail("collate_byname compare");
        // wcsxfrm used to answer 0 and transform nothing; in the C locale it is the identity.
        if (collate.transform(b.data(), b.data() + b.size()) != b) return fail("collate_byname transform");
        const auto& codecvt = std::use_facet<std::codecvt<wchar_t, char, std::mbstate_t>>(classic);
        const std::wstring wide = L"kumo";
        char narrow[16] = {};
        std::mbstate_t state{};
        const wchar_t* from_next = nullptr;
        char* to_next = nullptr;
        if (codecvt.out(state, wide.data(), wide.data() + wide.size(), from_next, narrow, narrow + sizeof narrow,
                        to_next) != std::codecvt_base::ok ||
            std::string(narrow, to_next) != "kumo")
            return fail("codecvt out");
        wchar_t back[16] = {};
        const char* narrow_next = nullptr;
        wchar_t* back_next = nullptr;
        state = std::mbstate_t{};
        if (codecvt.in(state, narrow, to_next, narrow_next, back, back + 16, back_next) != std::codecvt_base::ok ||
            std::wstring(back, back_next) != wide)
            return fail("codecvt in");
    }
    std::cout << "SH9-c7: wchar_t ctype, collate (C byname) and codecvt -> OK" << std::endl;

    // 4. wregex and wide std::format.
    {
        std::wsmatch parts;
        const std::wstring slice = L"SH9-c7-2026";
        if (!std::regex_match(slice, parts, std::wregex(LR"(SH9-c(\d)-(\d+))")) || parts[1] != L"7" ||
            parts[2] != L"2026")
            return fail("wregex");
        if (std::format(L"{:>4}|{}|{:.1f}", 42, L"x", 2.25) != L"  42|x|2.2") return fail("wide format");
    }
    std::cout << "SH9-c7: wregex and wide format -> OK" << std::endl;

    std::cout << "SH9-c7 wide libc++ green" << std::endl;
    return 0;
}
