// 이 파일은 오피스 화면에서 로컬 백엔드의 실행 상태를 확인하고 직접 제어한다.

import Darwin
import Foundation
import OfficeCore

enum OfficeBackendStatus: Equatable {
    case running
    case stopped
    case changing

    var isRunning: Bool {
        self == .running
    }

    var showsStoppedWarning: Bool {
        self == .stopped
    }

    var systemImage: String {
        switch self {
        case .running:
            "power"
        case .stopped:
            "power"
        case .changing:
            "arrow.triangle.2.circlepath"
        }
    }
}

struct OfficeBackendLaunchConfiguration: Equatable {
    static let jobLabel = "com.neo.office-backend-4317"

    let workdir: URL
    let backendDirectoryURL: URL
    let healthURL: URL
    let runtimeConfigurationURL: URL
    let releaseID: String
    let userID: uid_t

    var jobTarget: String {
        "gui/\(userID)/\(Self.jobLabel)"
    }
}

enum OfficeBackendRuntimeLocation {
    static func resolve(
        resourceURL: URL,
        workdir: URL,
        developmentRoot: URL? = nil,
        fileManager: FileManager = .default
    ) -> URL {
        let bundledBackend = resourceURL
            .appending(path: "OFFICESTRARuntime")
            .appending(path: "backend")
        if rustExecutable(backendDirectoryURL: bundledBackend, fileManager: fileManager) != nil {
            return bundledBackend
        }
        if let developmentRoot {
            let developmentBackend = developmentRoot.appending(path: "backend")
            if rustExecutable(backendDirectoryURL: developmentBackend, fileManager: fileManager) != nil {
                return developmentBackend
            }
        }
        return workdir.appending(path: "backend")
    }

    /// backendDirectoryURL은 기존 호출자의 런타임 위치 표식이다. 해당 디렉터리는 생성하지 않는다.
    /// 배포는 rust/bin, 개발 실행은 이 저장소 backend-rs의 release 실행 파일을 쓴다.
    static func rustExecutable(
        backendDirectoryURL: URL,
        fileManager: FileManager = .default
    ) -> URL? {
        let executable = backendDirectoryURL
            .deletingLastPathComponent()
            .appending(path: "rust")
            .appending(path: "bin")
            .appending(path: "officestra-backend")
        let developmentExecutable = backendDirectoryURL.deletingLastPathComponent()
            .appending(path: "backend-rs/target/release/officestra-backend")
        return [executable, developmentExecutable].first {
            fileManager.isExecutableFile(atPath: $0.path)
        }
    }
}

struct OfficeBackendLaunchCommand: Equatable {
    let executableURL: URL
    let arguments: [String]
}

enum OfficeBackendLaunchCommands {
    static func stop(
        configuration: OfficeBackendLaunchConfiguration
    ) -> OfficeBackendLaunchCommand {
        OfficeBackendLaunchCommand(
            executableURL: URL(fileURLWithPath: "/bin/launchctl"),
            arguments: ["bootout", configuration.jobTarget]
        )
    }

    static func restart(
        configuration: OfficeBackendLaunchConfiguration
    ) -> OfficeBackendLaunchCommand {
        OfficeBackendLaunchCommand(
            executableURL: URL(fileURLWithPath: "/bin/launchctl"),
            arguments: ["kickstart", "-k", configuration.jobTarget]
        )
    }

    /// 백엔드는 Rust만 실행한다. Node는 설치된 외부 CLI의 Playwright에만 선택적으로 넘긴다.
    static func start(
        configuration: OfficeBackendLaunchConfiguration,
        nodeExecutableURL: URL? = nil,
        rustExecutableURL: URL? = nil,
        standardOutputURL: URL? = nil,
        standardErrorURL: URL? = nil,
        executableSearchPaths: [String] = []
    ) -> OfficeBackendLaunchCommand {
        let path = ([nodeExecutableURL?.deletingLastPathComponent().path].compactMap { $0 }
            + executableSearchPaths
            + [
                "/opt/homebrew/bin",
                "/usr/local/bin",
                "/usr/bin",
                "/bin",
                "/usr/sbin",
                "/sbin",
            ])
            .reduce(into: [String]()) { result, element in
                if !result.contains(element) {
                    result.append(element)
                }
            }
            .joined(separator: ":")
        let outputRedirect = standardOutputURL.map {
            " >> \(shellQuoted($0.path))"
        } ?? ""
        let errorRedirect = standardErrorURL.map {
            " 2>> \(shellQuoted($0.path))"
        } ?? ""
        let logDirectories = [standardOutputURL, standardErrorURL]
            .compactMap { $0?.deletingLastPathComponent().path }
            .reduce(into: [String]()) { result, element in
                if !result.contains(element) {
                    result.append(element)
                }
            }
        let quotedLogDirectories = logDirectories
            .map(shellQuoted)
            .joined(separator: " ")
        let prepareLogs = logDirectories.isEmpty
            ? ""
            : "mkdir -p \(quotedLogDirectories); "
        let runtimeRoot = configuration.backendDirectoryURL.deletingLastPathComponent()
        let packagedDriver = runtimeRoot.appending(path: "tools/antigravity-playwright")
        let sourceDriver = runtimeRoot.appending(path: "backend/src/playwright-driver-1.57.0")
        let driver = !FileManager.default.fileExists(atPath: packagedDriver.appending(path: "package/cli.js").path)
            && FileManager.default.fileExists(atPath: sourceDriver.appending(path: "package/cli.js").path)
            ? sourceDriver : packagedDriver
        let rust = rustExecutableURL
            ?? OfficeBackendRuntimeLocation.rustExecutable(backendDirectoryURL: configuration.backendDirectoryURL)
            ?? runtimeRoot.appending(path: "rust/bin/officestra-backend")
        let externalNode = nodeExecutableURL.map {
            "export OFFICE_NODE_EXECUTABLE=\(shellQuoted($0.path)); "
        } ?? "unset OFFICE_NODE_EXECUTABLE; "
        let backendExecution = "export OFFICE_MIGRATIONS_DIR="
                + "\(shellQuoted(runtimeRoot.appending(path: "database/migrations").path)); "
                + "export OFFICE_RESULT_TOOL_DIR="
                + "\(shellQuoted(rust.deletingLastPathComponent().path)); "
                + "export OFFICE_PLAYWRIGHT_DRIVER_DIR="
                + "\(shellQuoted(driver.path)); "
                + externalNode
                + "cd \(shellQuoted(runtimeRoot.path)); "
                + "exec \(shellQuoted(rust.path))"
        let command = "umask 077; \(prepareLogs)"
            + "export PATH=\(shellQuoted(path)); "
            + "export OFFICE_WORKDIR=\(shellQuoted(configuration.workdir.path)); "
            + "export OFFICESTRA_RELEASE_ID="
            + "\(shellQuoted(configuration.releaseID)); "
            + "export CHARACTER_CONFIG_PATH="
            + "\(shellQuoted(configuration.runtimeConfigurationURL.path)); "
            + backendExecution
            + outputRedirect
            + errorRedirect
        return OfficeBackendLaunchCommand(
            executableURL: URL(fileURLWithPath: "/bin/launchctl"),
            arguments: [
                "submit",
                "-l",
                OfficeBackendLaunchConfiguration.jobLabel,
                "--",
                "/bin/zsh",
                "-lc",
                command,
            ]
        )
    }

    private static func shellQuoted(_ value: String) -> String {
        "'\(value.replacingOccurrences(of: "'", with: "'\\''"))'"
    }
}

@MainActor
final class OfficeBackendController: ObservableObject {
    @Published private(set) var status: OfficeBackendStatus = .changing
    @Published private(set) var errorMessage: String?

    private var configuration: OfficeBackendLaunchConfiguration?
    private var monitorTask: Task<Void, Never>?

    deinit {
        monitorTask?.cancel()
    }

    func activate(workdir: String, healthURL: URL) {
        guard configuration == nil else {
            return
        }
        guard let resourceURL = Bundle.main.resourceURL else {
            errorMessage = OfficeLocalization.string("앱 리소스 경로를 찾을 수 없습니다.")
            status = .stopped
            return
        }
        let workdirURL = URL(fileURLWithPath: workdir)
        let developmentRoot = Bundle.main.bundleURL.pathExtension == "app"
            ? nil
            : OfficeDevelopmentRuntimeLocator.locate()
        let layout = OfficeRuntimeLayout.resolve(
            resourceURL: resourceURL,
            developmentRoot: developmentRoot
        )
        configuration = OfficeBackendLaunchConfiguration(
            workdir: workdirURL,
            backendDirectoryURL: OfficeBackendRuntimeLocation.resolve(
                resourceURL: resourceURL,
                workdir: workdirURL,
                developmentRoot: developmentRoot
            ),
            healthURL: healthURL,
            runtimeConfigurationURL: layout.runtimeConfiguration,
            releaseID: OfficeAppReleaseIdentity.current(),
            userID: getuid()
        )
        monitorTask = Task { [weak self] in
            while !Task.isCancelled {
                await self?.refresh()
                try? await Task.sleep(for: .seconds(2))
            }
        }
    }

    func toggle() {
        guard let configuration, status != .changing else {
            return
        }
        let wasRunning = status.isRunning
        status = .changing
        errorMessage = nil

        Task {
            let failure = await Task.detached {
                Self.changeServerState(
                    wasRunning: wasRunning,
                    configuration: configuration
                )
            }.value
            if let failure {
                errorMessage = failure
            }
            try? await Task.sleep(for: .milliseconds(250))
            await refresh()
        }
    }

    func refresh() async {
        guard let configuration else {
            return
        }
        let isRunning = await Self.isHealthy(at: configuration.healthURL)
        guard status != .changing else {
            status = isRunning ? .running : .stopped
            return
        }
        status = isRunning ? .running : .stopped
    }

    private nonisolated static func changeServerState(
        wasRunning: Bool,
        configuration: OfficeBackendLaunchConfiguration
    ) -> String? {
        do {
            if wasRunning {
                try run(OfficeBackendLaunchCommands.stop(configuration: configuration))
            } else {
                do {
                    try run(
                        OfficeBackendLaunchCommands.restart(
                            configuration: configuration
                        )
                    )
                } catch {
                    try run(
                        OfficeBackendLaunchCommands.start(
                            configuration: configuration,
                            nodeExecutableURL: nodeExecutableURL(
                                configuration: configuration
                            ),
                            rustExecutableURL: OfficeBackendRuntimeLocation.rustExecutable(
                                backendDirectoryURL: configuration.backendDirectoryURL
                            ),
                            standardOutputURL: logURL(
                                named: "backend.out.log",
                                configuration: configuration
                            ),
                            standardErrorURL: logURL(
                                named: "backend.err.log",
                                configuration: configuration
                            ),
                            executableSearchPaths: cliSearchPaths()
                        )
                    )
                }
            }
            return nil
        } catch {
            return error.localizedDescription
        }
    }

    private nonisolated static func isHealthy(at url: URL) async -> Bool {
        do {
            let (_, response) = try await URLSession.shared.data(from: url)
            return (response as? HTTPURLResponse)?.statusCode == 200
        } catch {
            return false
        }
    }

    private nonisolated static func nodeExecutableURL(
        configuration: OfficeBackendLaunchConfiguration
    ) -> URL? {
        OfficeToolLocator.locate("node")
    }

    private nonisolated static func logURL(
        named name: String,
        configuration: OfficeBackendLaunchConfiguration
    ) -> URL {
        configuration.runtimeConfigurationURL
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .appending(path: "logs")
            .appending(path: name)
    }

    private nonisolated static func cliSearchPaths() -> [String] {
        AgentBackend.allCases.compactMap {
            OfficeToolLocator.locate($0.executableName)?
                .deletingLastPathComponent()
                .path
        }
    }

    private nonisolated static func run(
        _ command: OfficeBackendLaunchCommand
    ) throws {
        let process = Process()
        let stderr = Pipe()
        process.executableURL = command.executableURL
        process.arguments = command.arguments
        process.standardError = stderr
        process.standardInput = FileHandle.nullDevice
        try process.run()
        process.waitUntilExit()
        guard process.terminationStatus == 0 else {
            let output = stderr.fileHandleForReading.readDataToEndOfFile()
            let message = String(decoding: output, as: UTF8.self)
                .trimmingCharacters(in: .whitespacesAndNewlines)
            throw OfficeBackendControlError.launchctlFailed(
                message.isEmpty
                    ? OfficeLocalization.string("launchctl 실행에 실패했습니다.")
                    : OfficeLocalization.format("launchctl 실행에 실패했습니다: %@", message)
            )
        }
    }
}

private enum OfficeBackendControlError: LocalizedError {
    case nodeExecutableMissing
    case launchctlFailed(String)

    var errorDescription: String? {
        switch self {
        case .nodeExecutableMissing:
            OfficeLocalization.string("Node 실행 파일을 찾을 수 없습니다.")
        case let .launchctlFailed(message):
            message
        }
    }
}
