// OFFICESTRA 직원이 화면을 캡처하고 클릭·타이핑하는 데 쓰는 macOS 컴퓨터 사용 도우미 CLI다.
//
// 빌드
//   swiftc -O -o /tmp/officestra-cu scripts/officestra-computer-use.swift
//   clang -o /tmp/disclaim-run scripts/officestra-disclaim-run.c
// 사용
//   officestra-cu permissions                  화면 기록·손쉬운 사용 권한 상태
//   officestra-cu request                      macOS 권한 요청 창을 띄워 설정 목록에 항목을 만든다
//   officestra-cu screenshot <path.png> [--display N]
//                                              화면을 포인트 해상도 PNG로 저장(좌표는 좌상단 기준 포인트)
//   officestra-cu windows                      화면에 보이는 창 목록과 위치
//   officestra-cu frontmost                    맨 앞 앱과 그 앞 창 위치
//   officestra-cu open <앱 이름>               앱을 열고 맨 앞으로 가져온다
//   officestra-cu move <x> <y>                 마우스 이동
//   officestra-cu click <x> <y> [--right] [--double]
//   officestra-cu type <문자열>                 유니코드 문자열 타이핑(줄바꿈은 return 키)
//   officestra-cu key <조합>                    예: cmd+n, return, cmd+shift+n, esc
//
// 권한은 이 프로세스가 아니라 macOS가 정한 책임 프로세스에 걸린다. OFFICESTRA 일반 모드의 Claude 직원은 번들 node가
// 책임 프로세스라 권한이 없으므로, 권한이 허용된 nvm node를 disclaim-run으로 책임 분리해 띄운 뒤 그 아래에서 실행한다.
//   /tmp/disclaim-run /Users/neo/.nvm/versions/node/v24.14.0/bin/node -e \
//     "require('child_process').spawnSync('/tmp/officestra-cu',['permissions'],{stdio:'inherit'})"
// Codex 직원은 자체 Codex Computer Use 앱을 쓰므로 이 도우미는 Claude 직원 전용이다.

import AppKit
import ApplicationServices
import CoreGraphics
import Foundation
import ImageIO

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data("officestra-cu: \(message)\n".utf8))
    exit(1)
}

func requireAccessibility() {
    guard AXIsProcessTrusted() else {
        fail("손쉬운 사용 권한이 없어 입력을 보낼 수 없습니다. 시스템 설정 > 개인정보 보호 및 보안 > 손쉬운 사용에 백엔드 node 실행 파일을 추가하세요.")
    }
}

// MARK: - 권한

func reportPermissions() {
    let accessibility = AXIsProcessTrusted()
    let screenRecording = CGPreflightScreenCaptureAccess()
    print("accessibility=\(accessibility ? "granted" : "denied") screenRecording=\(screenRecording ? "granted" : "denied")")
    exit(accessibility && screenRecording ? 0 : 1)
}

// macOS에 권한 요청 창을 띄워 설정 목록에 책임 프로세스 항목이 생기게 한다.
func requestPermissions() {
    let options = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true] as CFDictionary
    let accessibility = AXIsProcessTrustedWithOptions(options)
    let screenRecording = CGRequestScreenCaptureAccess()
    print("accessibility=\(accessibility ? "granted" : "requested") screenRecording=\(screenRecording ? "granted" : "requested")")
    print("시스템 설정 > 개인정보 보호 및 보안의 손쉬운 사용·화면 기록 목록에서 이 도우미를 실행한 책임 앱(OFFICESTRA 또는 node) 항목을 켜세요.")
}

// MARK: - 화면 캡처

func activeDisplays() -> [CGDirectDisplayID] {
    var count: UInt32 = 0
    CGGetActiveDisplayList(0, nil, &count)
    var displays = [CGDirectDisplayID](repeating: 0, count: Int(count))
    CGGetActiveDisplayList(count, &displays, &count)
    return Array(displays.prefix(Int(count)))
}

// CGDisplayCreateImage는 macOS 15에서 폐기되어 시스템 screencapture로 원본 픽셀 PNG를 바로 받는다.
// 좌표계 일관성을 위해 저장 경로에 원본(레티나) 픽셀 그대로 쓰고, 화면의 포인트 크기를 함께 알린다.
func screenshot(path: String, displayIndex: Int) {
    guard CGPreflightScreenCaptureAccess() else {
        fail("화면 기록 권한이 없어 캡처할 수 없습니다. 시스템 설정 > 개인정보 보호 및 보안 > 화면 기록에 백엔드 node 실행 파일을 추가하세요.")
    }
    let displays = activeDisplays()
    guard displayIndex >= 0, displayIndex < displays.count else {
        fail("디스플레이 \(displayIndex)이(가) 없습니다. 연결된 디스플레이는 \(displays.count)개입니다.")
    }
    let bounds = CGDisplayBounds(displays[displayIndex])
    let process = Process()
    process.executableURL = URL(fileURLWithPath: "/usr/sbin/screencapture")
    process.arguments = ["-x", "-t", "png", "-D", String(displayIndex + 1), path]
    do {
        try process.run()
    } catch {
        fail("screencapture를 실행하지 못했습니다. \(error.localizedDescription)")
    }
    process.waitUntilExit()
    guard process.terminationStatus == 0 else {
        fail("화면을 캡처하지 못했습니다. screencapture 종료 코드 \(process.terminationStatus)")
    }
    var pixelWidth = Int(bounds.width)
    var pixelHeight = Int(bounds.height)
    if let source = CGImageSourceCreateWithURL(URL(fileURLWithPath: path) as CFURL, nil),
       let image = CGImageSourceCreateImageAtIndex(source, 0, nil) {
        pixelWidth = image.width
        pixelHeight = image.height
    }
    let scale = bounds.width > 0 ? Double(pixelWidth) / Double(bounds.width) : 1
    print("saved=\(path) pixels=\(pixelWidth)x\(pixelHeight) points=\(Int(bounds.width))x\(Int(bounds.height)) origin=\(Int(bounds.minX)),\(Int(bounds.minY)) scale=\(String(format: "%.1f", scale))")
}

// MARK: - 창 정보

struct WindowInfo {
    let owner: String
    let pid: Int
    let name: String
    let bounds: CGRect
}

func onScreenWindows() -> [WindowInfo] {
    let options: CGWindowListOption = [.optionOnScreenOnly, .excludeDesktopElements]
    guard let list = CGWindowListCopyWindowInfo(options, kCGNullWindowID) as? [[String: Any]] else {
        return []
    }
    return list.compactMap { entry in
        guard (entry[kCGWindowLayer as String] as? Int ?? 0) == 0 else { return nil }
        let rect = entry[kCGWindowBounds as String] as? [String: CGFloat] ?? [:]
        let bounds = CGRect(
            x: rect["X"] ?? 0,
            y: rect["Y"] ?? 0,
            width: rect["Width"] ?? 0,
            height: rect["Height"] ?? 0
        )
        guard bounds.width > 1, bounds.height > 1 else { return nil }
        return WindowInfo(
            owner: entry[kCGWindowOwnerName as String] as? String ?? "?",
            pid: entry[kCGWindowOwnerPID as String] as? Int ?? 0,
            name: entry[kCGWindowName as String] as? String ?? "",
            bounds: bounds
        )
    }
}

func describe(_ window: WindowInfo) -> String {
    let b = window.bounds
    return "\(window.owner) pid=\(window.pid) title=\"\(window.name)\" x=\(Int(b.minX)) y=\(Int(b.minY)) w=\(Int(b.width)) h=\(Int(b.height))"
}

func listWindows() {
    let windows = onScreenWindows()
    if windows.isEmpty {
        print("보이는 창이 없거나 창 정보를 읽을 권한이 없습니다.")
    }
    for window in windows {
        print(describe(window))
    }
}

func reportFrontmost() {
    guard let app = NSWorkspace.shared.frontmostApplication else {
        fail("맨 앞 앱을 알 수 없습니다.")
    }
    let pid = Int(app.processIdentifier)
    print("app=\(app.localizedName ?? "?") pid=\(pid) bundle=\(app.bundleIdentifier ?? "-")")
    if let window = onScreenWindows().first(where: { $0.pid == pid }) {
        print(describe(window))
    }
}

func openApplication(named name: String) {
    let process = Process()
    process.executableURL = URL(fileURLWithPath: "/usr/bin/open")
    process.arguments = ["-a", name]
    do {
        try process.run()
    } catch {
        fail("\(name)을(를) 열지 못했습니다. \(error.localizedDescription)")
    }
    process.waitUntilExit()
    guard process.terminationStatus == 0 else {
        fail("\(name)을(를) 열지 못했습니다. open 종료 코드 \(process.terminationStatus)")
    }
    usleep(800_000)
    reportFrontmost()
}

// MARK: - 마우스

func post(_ event: CGEvent?) {
    event?.post(tap: .cghidEventTap)
}

func moveMouse(to point: CGPoint) {
    post(CGEvent(mouseEventSource: nil, mouseType: .mouseMoved, mouseCursorPosition: point, mouseButton: .left))
}

func click(at point: CGPoint, right: Bool, double: Bool) {
    requireAccessibility()
    moveMouse(to: point)
    usleep(80_000)
    let down: CGEventType = right ? .rightMouseDown : .leftMouseDown
    let up: CGEventType = right ? .rightMouseUp : .leftMouseUp
    let button: CGMouseButton = right ? .right : .left
    let clicks = double ? 2 : 1
    for state in 1...clicks {
        let downEvent = CGEvent(mouseEventSource: nil, mouseType: down, mouseCursorPosition: point, mouseButton: button)
        downEvent?.setIntegerValueField(.mouseEventClickState, value: Int64(state))
        post(downEvent)
        usleep(40_000)
        let upEvent = CGEvent(mouseEventSource: nil, mouseType: up, mouseCursorPosition: point, mouseButton: button)
        upEvent?.setIntegerValueField(.mouseEventClickState, value: Int64(state))
        post(upEvent)
        usleep(70_000)
    }
    print("clicked x=\(Int(point.x)) y=\(Int(point.y)) button=\(right ? "right" : "left") count=\(clicks)")
}

// MARK: - 키보드

let keyCodes: [String: CGKeyCode] = [
    "a": 0, "s": 1, "d": 2, "f": 3, "h": 4, "g": 5, "z": 6, "x": 7, "c": 8, "v": 9,
    "b": 11, "q": 12, "w": 13, "e": 14, "r": 15, "y": 16, "t": 17,
    "1": 18, "2": 19, "3": 20, "4": 21, "6": 22, "5": 23, "=": 24, "9": 25, "7": 26,
    "-": 27, "8": 28, "0": 29, "]": 30, "o": 31, "u": 32, "[": 33, "i": 34, "p": 35,
    "return": 36, "enter": 36, "l": 37, "j": 38, "'": 39, "k": 40, ";": 41, "\\": 42,
    ",": 43, "/": 44, "n": 45, "m": 46, ".": 47, "tab": 48, "space": 49, "`": 50,
    "delete": 51, "backspace": 51, "esc": 53, "escape": 53,
    "f1": 122, "f2": 120, "f3": 99, "f4": 118, "f5": 96, "f6": 97, "f7": 98, "f8": 100,
    "f9": 101, "f10": 109, "f11": 103, "f12": 111,
    "home": 115, "end": 119, "pageup": 116, "pagedown": 121, "forwarddelete": 117,
    "left": 123, "right": 124, "down": 125, "up": 126,
]

let modifierFlags: [String: CGEventFlags] = [
    "cmd": .maskCommand, "command": .maskCommand,
    "shift": .maskShift,
    "alt": .maskAlternate, "option": .maskAlternate,
    "ctrl": .maskControl, "control": .maskControl,
]

func pressKey(code: CGKeyCode, flags: CGEventFlags) {
    let source = CGEventSource(stateID: .combinedSessionState)
    let down = CGEvent(keyboardEventSource: source, virtualKey: code, keyDown: true)
    down?.flags = flags
    post(down)
    usleep(30_000)
    let up = CGEvent(keyboardEventSource: source, virtualKey: code, keyDown: false)
    up?.flags = flags
    post(up)
    usleep(30_000)
}

func pressCombo(_ combo: String) {
    requireAccessibility()
    var flags: CGEventFlags = []
    var keyName: String?
    for part in combo.lowercased().split(separator: "+").map(String.init) {
        if let flag = modifierFlags[part] {
            flags.insert(flag)
        } else if keyName == nil {
            keyName = part
        } else {
            fail("키 조합 \(combo)을(를) 해석할 수 없습니다.")
        }
    }
    guard let name = keyName, let code = keyCodes[name] else {
        fail("알 수 없는 키 \(keyName ?? combo)입니다.")
    }
    pressKey(code: code, flags: flags)
    print("pressed \(combo)")
}

func typeText(_ text: String) {
    requireAccessibility()
    let source = CGEventSource(stateID: .combinedSessionState)
    var count = 0
    for character in text {
        if character == "\n" {
            pressKey(code: 36, flags: [])
            continue
        }
        var units = Array(String(character).utf16)
        let down = CGEvent(keyboardEventSource: source, virtualKey: 0, keyDown: true)
        down?.keyboardSetUnicodeString(stringLength: units.count, unicodeString: &units)
        post(down)
        let up = CGEvent(keyboardEventSource: source, virtualKey: 0, keyDown: false)
        up?.keyboardSetUnicodeString(stringLength: units.count, unicodeString: &units)
        post(up)
        usleep(12_000)
        count += 1
    }
    print("typed \(count) characters")
}

// MARK: - 진입점

func point(from arguments: [String]) -> CGPoint {
    guard arguments.count >= 2, let x = Double(arguments[0]), let y = Double(arguments[1]) else {
        fail("x y 좌표가 필요합니다.")
    }
    return CGPoint(x: x, y: y)
}

let arguments = Array(CommandLine.arguments.dropFirst())
guard let command = arguments.first else {
    fail("명령이 필요합니다. permissions | screenshot | windows | frontmost | open | move | click | type | key")
}
let rest = Array(arguments.dropFirst())

switch command {
case "permissions":
    reportPermissions()
case "request":
    requestPermissions()
case "screenshot":
    guard let path = rest.first else { fail("저장할 PNG 경로가 필요합니다.") }
    var displayIndex = 0
    if let flag = rest.firstIndex(of: "--display"), flag + 1 < rest.count, let index = Int(rest[flag + 1]) {
        displayIndex = index
    }
    screenshot(path: path, displayIndex: displayIndex)
case "windows":
    listWindows()
case "frontmost":
    reportFrontmost()
case "open":
    guard let name = rest.first, !name.isEmpty else { fail("앱 이름이 필요합니다.") }
    openApplication(named: name)
case "move":
    requireAccessibility()
    moveMouse(to: point(from: rest))
    print("moved")
case "click":
    click(at: point(from: rest), right: rest.contains("--right"), double: rest.contains("--double"))
case "type":
    guard rest.count >= 1 else { fail("타이핑할 문자열이 필요합니다.") }
    typeText(rest.joined(separator: " "))
case "key":
    guard let combo = rest.first else { fail("키 조합이 필요합니다. 예: cmd+n") }
    pressCombo(combo)
default:
    fail("알 수 없는 명령 \(command)입니다.")
}
