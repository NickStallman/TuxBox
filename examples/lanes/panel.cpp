// Samples the presented window to see TuxBox's performance panel over the game.
// The window and screen functions are looked up at run time, so the guest's
// import table stays like a console program's.
#include "panel.h"
#include "game.h"
#include "screen.h"
#include <windows.h>
#include <cstdio>
#include <cstdlib>
#include <stdexcept>

namespace {
template <typename F> F find(const wchar_t* library, const char* name) {
    HMODULE module = LoadLibraryW(library);
    auto function = module ? reinterpret_cast<F>(GetProcAddress(module, name)) : nullptr;
    if (!function) throw std::runtime_error(std::string("missing host function ") + name);
    return function;
}
bool close_to(COLORREF colour, int red, int green, int blue) {
    return colour != CLR_INVALID && std::abs(GetRValue(colour) - red) <= 3 &&
           std::abs(GetGValue(colour) - green) <= 3 && std::abs(GetBValue(colour) - blue) <= 3;
}
}

void expect_performance_panel(Screen& screen, const Game& game) {
    auto enumerate = find<decltype(&EnumWindows)>(L"user32.dll", "EnumWindows");
    auto owner_of = find<decltype(&GetWindowThreadProcessId)>(L"user32.dll", "GetWindowThreadProcessId");
    auto visible = find<decltype(&IsWindowVisible)>(L"user32.dll", "IsWindowVisible");
    auto raise = find<decltype(&SetWindowPos)>(L"user32.dll", "SetWindowPos");
    auto client_rect = find<decltype(&GetClientRect)>(L"user32.dll", "GetClientRect");
    auto to_screen = find<decltype(&ClientToScreen)>(L"user32.dll", "ClientToScreen");
    auto screen_dc = find<decltype(&GetDC)>(L"user32.dll", "GetDC");
    auto release_dc = find<decltype(&ReleaseDC)>(L"user32.dll", "ReleaseDC");
    auto pixel = find<decltype(&GetPixel)>(L"gdi32.dll", "GetPixel");
    // GetPixel reads physical pixels; Lanes, like a title, is not DPI aware and sees
    // its window in logical pixels once Windows scales the display. Hosts without
    // display scaling may lack the conversion.
    using Convert = BOOL(WINAPI*)(HWND, POINT*);
    HMODULE user32 = LoadLibraryW(L"user32.dll");
    auto physical = user32 ? reinterpret_cast<Convert>(
        GetProcAddress(user32, "LogicalToPhysicalPointForPerMonitorDPI")) : nullptr;

    struct Search {
        decltype(owner_of) owner;
        decltype(visible) shown;
        HWND window;
    } search{owner_of, visible, nullptr};
    COLORREF found[2]{};
    const auto deadline = GetTickCount64() + 3000;
    while (GetTickCount64() < deadline) {
        // Each draw presents a frame, which also runs the window's messages.
        if (!screen.draw(game, 0, 0, false, true)) throw std::runtime_error("the window closed");
        if (!search.window) {
            enumerate([](HWND window, LPARAM data) -> BOOL {
                auto* wanted = reinterpret_cast<Search*>(data);
                DWORD process = 0;
                wanted->owner(window, &process);
                if (process != GetCurrentProcessId() || !wanted->shown(window)) return TRUE;
                wanted->window = window;
                return FALSE;
            }, reinterpret_cast<LPARAM>(&search));
            if (search.window)
                raise(search.window, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW);
            continue;
        }
        RECT client{};
        POINT origin{};
        if (!client_rect(search.window, &client) || !to_screen(search.window, &origin)) continue;
        HDC display = screen_dc(nullptr);
        if (!display) throw std::runtime_error("could not acquire the screen");
        // Two points of the panel's background, left of its text: 5 pixels into a
        // panel 16 pixels from the top left of the frame.
        constexpr int frame_x = 21, frame_y[2] = {40, 200};
        for (int i = 0; i < 2; ++i) {
            POINT at{origin.x + client.right * frame_x / Screen::width,
                     origin.y + client.bottom * frame_y[i] / Screen::height};
            if (physical) physical(search.window, &at);
            found[i] = pixel(display, at.x, at.y);
        }
        release_dc(nullptr, display);
        if (close_to(found[0], 0x0e, 0x0e, 0x14) && close_to(found[1], 0x0e, 0x0e, 0x14)) return;
        Sleep(16);
    }
    char message[160];
    std::snprintf(message, sizeof(message), "performance panel not on screen: sampled %#lx and %#lx",
                  static_cast<unsigned long>(found[0]), static_cast<unsigned long>(found[1]));
    throw std::runtime_error(message);
}
