// 이 파일은 백엔드 중지와 재기동에 쓰는 launchctl 명령 구성을 검증한다.

import Foundation
import XCTest
@testable import OfficeGame

final class OfficeBackendLaunchCommandsTests: XCTestCase {
    private let configuration = OfficeBackendLaunchConfiguration(
        workdir: URL(fileURLWithPath: "/Users/example/project"),
        backendDirectoryURL: URL(
            fileURLWithPath: "/Applications/OFFICESTRA.app/Contents/Resources/OFFICESTRARuntime/backend"
        ),
        healthURL: URL(string: "http://127.0.0.1:4317/health")!,
        runtimeConfigurationURL: URL(
            fileURLWithPath: "/Applications/OFFICESTRA.app/Contents/Resources/OfficeLLM_OfficeCore.bundle/characters.json"
        ),
        releaseID: "release-test",
        userID: 501
    )

    func testStopBootsOutOnlyTheOfficeBackendJob() {
        let command = OfficeBackendLaunchCommands.stop(
            configuration: configuration
        )

        XCTAssertEqual(command.executableURL.path, "/bin/launchctl")
        XCTAssertEqual(
            command.arguments,
            ["bootout", "gui/501/com.neo.office-backend-4317"]
        )
    }

    func testRestartTargetsTheExistingOfficeBackendJob() {
        let command = OfficeBackendLaunchCommands.restart(
            configuration: configuration
        )

        XCTAssertEqual(
            command.arguments,
            ["kickstart", "-k", "gui/501/com.neo.office-backend-4317"]
        )
    }

    func testStartSubmitsOfficeBackendWithRuntimeConfiguration() {
        let command = OfficeBackendLaunchCommands.start(
            configuration: configuration,
            nodeExecutableURL: URL(fileURLWithPath: "/opt/homebrew/bin/node")
        )

        XCTAssertEqual(command.arguments.prefix(5), [
            "submit",
            "-l",
            "com.neo.office-backend-4317",
            "--",
            "/bin/zsh",
        ])
        XCTAssertTrue(command.arguments.last?.contains(
            "CHARACTER_CONFIG_PATH='/Applications/OFFICESTRA.app/Contents/Resources/OfficeLLM_OfficeCore.bundle/characters.json'"
        ) == true)
        XCTAssertTrue(command.arguments.last?.contains(
            "OFFICE_WORKDIR='/Users/example/project'"
        ) == true)
        XCTAssertTrue(command.arguments.last?.contains(
            "OFFICESTRA_RELEASE_ID='release-test'"
        ) == true)
        XCTAssertTrue(command.arguments.last?.contains(
            "cd '/Applications/OFFICESTRA.app/Contents/Resources/OFFICESTRARuntime'"
        ) == true)
    }

    func testStartRunsRustBackendWithBundledRuntimePaths() throws {
        let runtime = "/Applications/OFFICESTRA.app/Contents/Resources/OFFICESTRARuntime"
        let launch = OfficeBackendLaunchCommands.start(
            configuration: configuration,
            nodeExecutableURL: URL(fileURLWithPath: "\(runtime)/node/bin/node"),
            rustExecutableURL: URL(fileURLWithPath: "\(runtime)/rust/bin/officestra-backend"),
            standardOutputURL: URL(fileURLWithPath: "/tmp/officestra-logs/backend.out.log"),
            standardErrorURL: URL(fileURLWithPath: "/tmp/officestra-logs/backend.err.log")
        )
        let command = try XCTUnwrap(launch.arguments.last)

        XCTAssertTrue(command.hasPrefix("umask 077; mkdir -p '/tmp/officestra-logs'; export PATH='\(runtime)/node/bin:"))
        XCTAssertTrue(command.contains(
            "export OFFICE_WORKDIR='/Users/example/project'; "
                + "export OFFICESTRA_RELEASE_ID='release-test'; "
                + "export CHARACTER_CONFIG_PATH='/Applications/OFFICESTRA.app/Contents/Resources/OfficeLLM_OfficeCore.bundle/characters.json'; "
        ))
        XCTAssertTrue(command.hasSuffix(
            "export OFFICE_MIGRATIONS_DIR='\(runtime)/database/migrations'; "
                + "export OFFICE_RESULT_TOOL_DIR='\(runtime)/rust/bin'; "
                + "export OFFICE_PLAYWRIGHT_DRIVER_DIR='\(runtime)/tools/antigravity-playwright'; "
                + "export OFFICE_NODE_EXECUTABLE='\(runtime)/node/bin/node'; "
                + "cd '\(runtime)'; "
                + "exec '\(runtime)/rust/bin/officestra-backend'"
                + " >> '/tmp/officestra-logs/backend.out.log'"
                + " 2>> '/tmp/officestra-logs/backend.err.log'"
        ))
        XCTAssertFalse(command.contains("server.mjs"))

        let syntaxCheck = Process()
        syntaxCheck.executableURL = URL(fileURLWithPath: "/bin/zsh")
        syntaxCheck.arguments = ["-n", "-c", command]
        syntaxCheck.standardOutput = FileHandle.nullDevice
        syntaxCheck.standardError = FileHandle.nullDevice
        try syntaxCheck.run()
        syntaxCheck.waitUntilExit()
        XCTAssertEqual(syntaxCheck.terminationStatus, 0)
    }

    func testRustExecutableIsUsedOnlyWhenBundledAndExecutable() throws {
        let runtime = FileManager.default.temporaryDirectory
            .appending(path: UUID().uuidString)
            .appending(path: "OFFICESTRARuntime")
        let backend = runtime.appending(path: "backend")
        let rust = runtime.appending(path: "rust/bin/officestra-backend")
        try FileManager.default.createDirectory(
            at: rust.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        defer {
            try? FileManager.default.removeItem(
                at: runtime.deletingLastPathComponent()
            )
        }

        XCTAssertNil(OfficeBackendRuntimeLocation.rustExecutable(
            backendDirectoryURL: backend
        ))
        XCTAssertTrue(FileManager.default.createFile(
            atPath: rust.path,
            contents: Data()
        ))
        XCTAssertNil(
            OfficeBackendRuntimeLocation.rustExecutable(backendDirectoryURL: backend),
            "실행 권한이 없으면 설치 오류이며 Node로 우회하지 않는다"
        )
        try FileManager.default.setAttributes(
            [.posixPermissions: 0o755],
            ofItemAtPath: rust.path
        )
        XCTAssertEqual(
            OfficeBackendRuntimeLocation.rustExecutable(backendDirectoryURL: backend)?.path,
            rust.path
        )
    }

    func testBundledBackendIsPreferredOverWorkspaceBackend() throws {
        let root = FileManager.default.temporaryDirectory
            .appending(path: UUID().uuidString)
        let resources = root.appending(path: "Resources")
        let bundledBackend = resources
            .appending(path: "OFFICESTRARuntime")
            .appending(path: "backend")
        let server = bundledBackend.deletingLastPathComponent()
            .appending(path: "rust/bin/officestra-backend")
        let workdir = root.appending(path: "SelectedProject")
        try FileManager.default.createDirectory(
            at: server.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        XCTAssertTrue(FileManager.default.createFile(
            atPath: server.path,
            contents: Data()
        ))
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: server.path)
        defer { try? FileManager.default.removeItem(at: root) }

        XCTAssertEqual(
            OfficeBackendRuntimeLocation.resolve(
                resourceURL: resources,
                workdir: workdir
            ),
            bundledBackend
        )
    }

    func testSourceBackendIsUsedForSwiftRunWithAnotherWorkspace() throws {
        let root = FileManager.default.temporaryDirectory
            .appending(path: UUID().uuidString)
        let resources = root.appending(path: "build/resources")
        let sourceRoot = root.appending(path: "OFFICESTRA")
        let server = sourceRoot.appending(path: "backend-rs/target/release/officestra-backend")
        let workdir = root.appending(path: "SelectedProject")
        try FileManager.default.createDirectory(
            at: server.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        XCTAssertTrue(FileManager.default.createFile(
            atPath: server.path,
            contents: Data()
        ))
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: server.path)
        defer { try? FileManager.default.removeItem(at: root) }

        XCTAssertEqual(
            OfficeBackendRuntimeLocation.resolve(
                resourceURL: resources,
                workdir: workdir,
                developmentRoot: sourceRoot
            ),
            sourceRoot.appending(path: "backend")
        )
    }

    func testSourceCheckoutBackendRemainsDevelopmentFallback() {
        let resources = URL(fileURLWithPath: "/tmp/missing-resources")
        let workdir = URL(fileURLWithPath: "/Users/example/office")

        XCTAssertEqual(
            OfficeBackendRuntimeLocation.resolve(
                resourceURL: resources,
                workdir: workdir
            ),
            workdir.appending(path: "backend")
        )
    }

    func testRustStartsWithoutNodeAndNeverFallsBackToJavaScript() throws {
        let command = try XCTUnwrap(OfficeBackendLaunchCommands.start(
            configuration: configuration
        ).arguments.last)
        XCTAssertTrue(command.contains("unset OFFICE_NODE_EXECUTABLE;"))
        XCTAssertTrue(command.contains("rust/bin/officestra-backend'"))
        XCTAssertFalse(command.contains("server.mjs"))
        XCTAssertFalse(command.contains("node/bin/node"))
    }

    func testDevelopmentUsesCachedThirdPartyDriverWithoutNodeBackend() throws {
        let root = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let driver = root.appending(path: "backend/src/playwright-driver-1.57.0")
        try FileManager.default.createDirectory(at: driver.appending(path: "package"), withIntermediateDirectories: true)
        XCTAssertTrue(FileManager.default.createFile(atPath: driver.appending(path: "package/cli.js").path, contents: Data()))
        let development = OfficeBackendLaunchConfiguration(
            workdir: root, backendDirectoryURL: root.appending(path: "backend"),
            healthURL: configuration.healthURL, runtimeConfigurationURL: configuration.runtimeConfigurationURL,
            releaseID: "test", userID: 501)
        let command = try XCTUnwrap(OfficeBackendLaunchCommands.start(configuration: development).arguments.last)
        XCTAssertTrue(command.contains("OFFICE_PLAYWRIGHT_DRIVER_DIR='\(driver.path)'"))
        XCTAssertFalse(command.contains("server.mjs"))
    }
}
