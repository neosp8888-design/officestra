import Foundation
import XCTest

final class OfficeApplicationStartupTests: XCTestCase {
    func testSwiftUILaunchUsesProductionEventDispatcher() throws {
        let root = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let temporary = FileManager.default.temporaryDirectory
            .appendingPathComponent("office-startup-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: temporary, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: temporary) }
        let binary = temporary.appendingPathComponent("probe")
        let sources = [
            "Sources/OfficeGame/OfficeApplicationMain.swift",
            "Sources/OfficeGame/ConversationPointerMoveCoalescer.swift",
            "Sources/OfficeGame/ConversationScrollWheelCoalescer.swift",
            "Sources/OfficeGame/ConversationScrollEdgeGate.swift",
            "Tests/Fixtures/OfficeApplicationLaunchProbe.swift",
        ].map { root.appendingPathComponent($0).path }
        try run("/usr/bin/xcrun", arguments: ["swiftc", "-parse-as-library"] + sources + ["-o", binary.path],
                log: temporary.appendingPathComponent("compile.log"), timeout: 60)
        let log = temporary.appendingPathComponent("launch.log")
        try run(binary.path, arguments: [], log: log, timeout: 15)
        let result = try String(contentsOf: log, encoding: .utf8)
        XCTAssertTrue(result.contains("CUSTOM_DISPATCHER=true"), result)
        XCTAssertTrue(result.contains("CUSTOM_SEND_EVENT=true"), result)
        print("[application-startup] Fresh SwiftUI process uses OfficeApplication.sendEvent")
    }

    private func run(_ executable: String, arguments: [String], log: URL, timeout: TimeInterval) throws {
        FileManager.default.createFile(atPath: log.path, contents: nil)
        let output = try FileHandle(forWritingTo: log)
        defer { try? output.close() }
        let process = Process()
        process.executableURL = URL(fileURLWithPath: executable)
        process.arguments = arguments
        process.standardOutput = output
        process.standardError = output
        let done = DispatchSemaphore(value: 0)
        process.terminationHandler = { _ in done.signal() }
        try process.run()
        if done.wait(timeout: .now() + timeout) == .timedOut {
            process.terminate()
            XCTFail("Startup validation timed out: \(executable)")
            return
        }
        let details = (try? String(contentsOf: log, encoding: .utf8)) ?? ""
        XCTAssertEqual(process.terminationStatus, 0, details)
    }
}
