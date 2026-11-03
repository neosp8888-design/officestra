// 이 파일은 새 Mac에서 DB·백엔드·AI CLI를 확인하고 첫 업무 전까지 자동 준비한다.

import Darwin
import Foundation
import OfficeCore

enum OfficeSetupCheck: Equatable, Sendable {
    case pending
    case checking(String)
    case ready(String)
    case actionRequired(String)
    case failed(String)

    var isReady: Bool {
        if case .ready = self {
            return true
        }
        return false
    }
}

struct OfficeSetupSnapshot: Equatable, Sendable {
    let workspace: String
    var runtime: OfficeSetupCheck = .pending
    var docker: OfficeSetupCheck = .pending
    var database: OfficeSetupCheck = .pending
    var backend: OfficeSetupCheck = .pending
    var codex: OfficeSetupCheck = .pending
    var claude: OfficeSetupCheck = .pending
    var antigravity: OfficeSetupCheck = .pending
    var dockerDataSelection: OfficeDockerDataSelection?
    var providerRemapTarget: AgentBackend?
    var backendReplacement: OfficeBackendReplacement?
}

enum OfficeDockerDataSelection: Equatable, Sendable {
    case legacyOnly
    case currentAndLegacy
}

enum OfficeBackendReplacement: Equatable, Sendable {
    case legacy
    case wrongWorkspace(String)
    case differentRelease(String?)
    case unresponsive

    var actionTitle: String {
        switch self {
        case .legacy:
            OfficeLocalization.string("구형 백엔드를 안전하게 교체")
        case .wrongWorkspace:
            OfficeLocalization.string("이 프로젝트 백엔드로 전환")
        case .differentRelease:
            OfficeLocalization.string("이 앱에 포함된 백엔드로 안전하게 전환")
        case .unresponsive:
            OfficeLocalization.string("기존 백엔드를 안전하게 확인")
        }
    }
}

enum OfficeSetupResult: Equatable, Sendable {
    case ready(
        snapshot: OfficeSetupSnapshot,
        availableBackends: Set<AgentBackend>,
        executablePaths: [AgentBackend: String]
    )
    case needsAction(OfficeSetupSnapshot)
    case failed(snapshot: OfficeSetupSnapshot, message: String)
}

struct OfficeRuntimeLayout: Equatable, Sendable {
    let runtimeRoot: URL
    let backendDirectory: URL
    let nodeExecutable: URL
    let composeFile: URL
    let bundledConfiguration: URL
    let supportDirectory: URL
    let runtimeConfiguration: URL
    let standardOutputLog: URL
    let standardErrorLog: URL

    static func resolve(
        resourceURL: URL,
        supportDirectory: URL? = nil,
        developmentRoot: URL? = nil,
        developmentNodeExecutable: URL? = nil,
        fileManager: FileManager = .default
    ) -> OfficeRuntimeLayout {
        let supportRoot = supportDirectory
            ?? fileManager.urls(
                for: .applicationSupportDirectory,
                in: .userDomainMask
            ).first!
                .appending(path: "OFFICESTRA")
        let logs = supportRoot.appending(path: "logs")
        if let developmentRoot {
            let nodeExecutable = developmentNodeExecutable
                ?? OfficeToolLocator.locate(
                    "node",
                    fileManager: fileManager
                )
                ?? developmentRoot.appending(path: ".missing-node")
            return OfficeRuntimeLayout(
                runtimeRoot: developmentRoot,
                backendDirectory: developmentRoot.appending(path: "backend"),
                nodeExecutable: nodeExecutable,
                composeFile: developmentRoot
                    .appending(path: "infra")
                    .appending(path: "compose.yaml"),
                bundledConfiguration: resourceURL
                    .appending(path: "OfficeLLM_OfficeCore.bundle")
                    .appending(path: "characters.json"),
                supportDirectory: supportRoot,
                runtimeConfiguration: supportRoot
                    .appending(path: "runtime")
                    .appending(path: "characters.json"),
                standardOutputLog: logs.appending(path: "backend.out.log"),
                standardErrorLog: logs.appending(path: "backend.err.log")
            )
        }

        let runtimeRoot = resourceURL.appending(path: "OFFICESTRARuntime")
        return OfficeRuntimeLayout(
            runtimeRoot: runtimeRoot,
            backendDirectory: runtimeRoot.appending(path: "backend"),
            nodeExecutable: runtimeRoot
                .appending(path: "node")
                .appending(path: "bin")
                .appending(path: "node"),
            composeFile: runtimeRoot
                .appending(path: "infra")
                .appending(path: "compose.yaml"),
            bundledConfiguration: resourceURL
                .appending(path: "OfficeLLM_OfficeCore.bundle")
                .appending(path: "characters.json"),
            supportDirectory: supportRoot,
            runtimeConfiguration: supportRoot
                .appending(path: "runtime")
                .appending(path: "characters.json"),
            standardOutputLog: logs.appending(path: "backend.out.log"),
            standardErrorLog: logs.appending(path: "backend.err.log")
        )
    }

    func validate(fileManager: FileManager = .default) throws {
        let requiredFiles = [
            backendDirectory.appending(path: "src/server.mjs"),
            nodeExecutable,
            composeFile,
            bundledConfiguration,
        ]
        guard requiredFiles.allSatisfy({
            fileManager.fileExists(atPath: $0.path)
        }) else {
            throw OfficeSetupError.runtimeIncomplete
        }
        guard fileManager.isExecutableFile(atPath: nodeExecutable.path) else {
            throw OfficeSetupError.bundledNodeNotExecutable
        }
    }
}

enum OfficeDevelopmentRuntimeLocator {
    static func locate(
        startingAt directories: [URL]? = nil,
        fileManager: FileManager = .default
    ) -> URL? {
        let startingDirectories = directories ?? [
            URL(fileURLWithPath: fileManager.currentDirectoryPath),
            URL(fileURLWithPath: CommandLine.arguments[0])
                .deletingLastPathComponent(),
        ]
        var visited = Set<String>()
        for startingDirectory in startingDirectories {
            var candidate = startingDirectory
                .standardizedFileURL
                .resolvingSymlinksInPath()
            while visited.insert(candidate.path).inserted {
                let requiredPaths = [
                    candidate.appending(path: "backend/src/server.mjs"),
                    candidate.appending(path: "infra/compose.yaml"),
                    candidate.appending(path: "Package.swift"),
                ]
                if requiredPaths.allSatisfy({
                    fileManager.fileExists(atPath: $0.path)
                }) {
                    return candidate
                }
                let parent = candidate.deletingLastPathComponent()
                if parent.path == candidate.path {
                    break
                }
                candidate = parent
            }
        }
        return nil
    }
}

struct OfficeProcessResult: Equatable, Sendable {
    let exitCode: Int32
    let output: String

    var succeeded: Bool {
        exitCode == 0
    }
}

enum OfficeProcessRunner {
    static func run(
        executableURL: URL,
        arguments: [String],
        environment: [String: String]? = nil,
        timeout: TimeInterval = 15
    ) async -> OfficeProcessResult {
        await Task.detached {
            runSynchronously(
                executableURL: executableURL,
                arguments: arguments,
                environment: environment,
                timeout: timeout
            )
        }.value
    }

    private static func runSynchronously(
        executableURL: URL,
        arguments: [String],
        environment: [String: String]?,
        timeout: TimeInterval
    ) -> OfficeProcessResult {
        let process = Process()
        let outputURL = FileManager.default.temporaryDirectory
            .appending(path: "officestra-process-\(UUID().uuidString).log")
        guard FileManager.default.createFile(
            atPath: outputURL.path,
            contents: nil
        ), let output = try? FileHandle(forWritingTo: outputURL) else {
            return OfficeProcessResult(
                exitCode: -1,
                output: "명령 결과를 기록할 임시 파일을 만들 수 없습니다."
            )
        }
        defer {
            try? output.close()
            try? FileManager.default.removeItem(at: outputURL)
        }
        process.executableURL = executableURL
        process.arguments = arguments
        process.environment = environment
        process.standardOutput = output
        process.standardError = output
        process.standardInput = FileHandle.nullDevice

        let completed = DispatchSemaphore(value: 0)
        process.terminationHandler = { _ in
            completed.signal()
        }
        do {
            try process.run()
        } catch {
            return OfficeProcessResult(
                exitCode: -1,
                output: error.localizedDescription
            )
        }

        if completed.wait(timeout: .now() + timeout) == .timedOut {
            process.terminate()
            _ = completed.wait(timeout: .now() + 2)
            return OfficeProcessResult(
                exitCode: -2,
                output: "명령 실행 시간이 초과됐습니다."
            )
        }
        try? output.synchronize()
        let data = (try? Data(contentsOf: outputURL)) ?? Data()
        return OfficeProcessResult(
            exitCode: process.terminationStatus,
            output: String(decoding: data, as: UTF8.self)
                .trimmingCharacters(in: .whitespacesAndNewlines)
        )
    }
}

enum OfficeToolLocator {
    static func locate(
        _ name: String,
        fileManager: FileManager = .default,
        homeDirectory: URL? = nil,
        searchPath: String? = ProcessInfo.processInfo.environment["PATH"]
    ) -> URL? {
        let home = homeDirectory ?? fileManager.homeDirectoryForCurrentUser
        let searchPathCandidates = String(searchPath ?? "")
            .split(separator: ":")
            .map { URL(fileURLWithPath: String($0)).appending(path: name) }
        var candidates = [
            home.appending(path: ".local/bin/\(name)"),
            home.appending(path: ".volta/bin/\(name)"),
            home.appending(path: ".asdf/shims/\(name)"),
            home.appending(path: ".local/share/mise/shims/\(name)"),
        ] + searchPathCandidates + [
            URL(fileURLWithPath: "/opt/homebrew/bin/\(name)"),
            URL(fileURLWithPath: "/usr/local/bin/\(name)"),
        ]
        let nvmRoot = home.appending(path: ".nvm/versions/node")
        if let versions = try? fileManager.contentsOfDirectory(
            at: nvmRoot,
            includingPropertiesForKeys: nil
        ) {
            candidates.insert(
                contentsOf: versions
                    .sorted {
                        $0.lastPathComponent.compare(
                            $1.lastPathComponent,
                            options: .numeric
                        ) == .orderedDescending
                    }
                    .map { $0.appending(path: "bin/\(name)") },
                at: 0
            )
        }
        return candidates.first(where: {
            isRegularExecutable($0, fileManager: fileManager)
        })
    }

    static func docker(
        fileManager: FileManager = .default,
        homeDirectory: URL? = nil,
        searchPath: String? = ProcessInfo.processInfo.environment["PATH"]
    ) -> URL? {
        let home = homeDirectory ?? fileManager.homeDirectoryForCurrentUser
        let searchPathCandidates = String(searchPath ?? "")
            .split(separator: ":")
            .map { URL(fileURLWithPath: String($0)).appending(path: "docker") }
        let candidates = [
            home.appending(path: ".docker/bin/docker"),
        ] + searchPathCandidates + [
            URL(fileURLWithPath: "/Applications/Docker.app/Contents/Resources/bin/docker"),
            URL(fileURLWithPath: "/opt/homebrew/bin/docker"),
            URL(fileURLWithPath: "/usr/local/bin/docker"),
        ]
        return candidates.first(where: {
            isRegularExecutable($0, fileManager: fileManager)
        })
    }

    private static func isRegularExecutable(
        _ url: URL,
        fileManager: FileManager
    ) -> Bool {
        var isDirectory = ObjCBool(false)
        guard fileManager.fileExists(
            atPath: url.path,
            isDirectory: &isDirectory
        ), !isDirectory.boolValue else {
            return false
        }
        return fileManager.isExecutableFile(atPath: url.path)
    }
}

struct OfficeCLIProbe: Equatable, Sendable {
    let backend: AgentBackend
    let executableURL: URL?
    let authenticated: Bool
    let version: String?
}

enum OfficeSetupReadyStateIssueKind: Equatable, Sendable {
    case cliPathSynchronization
    case providerCompatibility
}

struct OfficeSetupReadyStateIssue: Equatable, Sendable {
    let kind: OfficeSetupReadyStateIssueKind
    let message: String

    var allowsProviderRemap: Bool {
        kind == .providerCompatibility
    }
}

enum OfficeCLIInspector {
    static func inspect(
        backend: AgentBackend,
        environment: [String: String]
    ) async -> OfficeCLIProbe {
        guard let executable = OfficeToolLocator.locate(backend.executableName) else {
            return OfficeCLIProbe(
                backend: backend,
                executableURL: nil,
                authenticated: false,
                version: nil
            )
        }
        async let versionResult = OfficeProcessRunner.run(
            executableURL: executable,
            arguments: ["--version"],
            environment: environment,
            timeout: 8
        )
        let authenticationArguments: [String]
        switch backend {
        case .codex:
            authenticationArguments = ["login", "status"]
        case .claude:
            authenticationArguments = ["auth", "status", "--json"]
        case .antigravity:
            authenticationArguments = ["models"]
        }
        async let authenticationResult = OfficeProcessRunner.run(
            executableURL: executable,
            arguments: authenticationArguments,
            environment: environment,
            timeout: 10
        )
        let (version, authentication) = await (
            versionResult,
            authenticationResult
        )
        return OfficeCLIProbe(
            backend: backend,
            executableURL: executable,
            authenticated: authentication.succeeded,
            version: version.succeeded
                ? version.output.split(separator: "\n").first.map(String.init)
                : nil
        )
    }

    static func check(for probe: OfficeCLIProbe) -> OfficeSetupCheck {
        guard probe.executableURL != nil else {
            return .actionRequired("설치가 필요합니다.")
        }
        guard probe.authenticated else {
            return .actionRequired("로그인이 필요합니다.")
        }
        return .ready(probe.version ?? "로그인됨")
    }
}

struct OfficeBackendHealth: Decodable, Equatable, Sendable {
    struct Database: Decodable, Equatable, Sendable {
        let ok: Bool
    }

    let ok: Bool
    let service: String?
    let apiVersion: Int?
    let pid: Int?
    let releaseID: String?
    let activeTurnCount: Int?
    let acceptingJobs: Bool?
    let draining: Bool?
    let idle: Bool?
    let workdir: String?
    let database: Database?
}

enum OfficeAppReleaseIdentity {
    static func current(bundle: Bundle = .main) -> String {
        if let releaseID = bundle.object(
            forInfoDictionaryKey: "OFFICESTRABackendReleaseID"
        ) as? String,
           !releaseID.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        {
            return releaseID
        }
        let version = bundle.object(
            forInfoDictionaryKey: "CFBundleShortVersionString"
        ) as? String ?? "development"
        let build = bundle.object(
            forInfoDictionaryKey: "CFBundleVersion"
        ) as? String
        return build.map { "\(version)+\($0)" } ?? version
    }
}

enum OfficeBackendConnection: Equatable, Sendable {
    case unavailable
    case ready
    case legacy
    case wrongWorkspace(String)
    case versionMismatch(String?)
    case foreignService
}

enum OfficeBackendHealthProbe {
    static func classify(
        _ health: OfficeBackendHealth?,
        expectedWorkdir: String,
        expectedReleaseID: String? = nil
    ) -> OfficeBackendConnection {
        guard let health else {
            return .unavailable
        }
        guard let service = health.service else {
            return health.ok ? .legacy : .foreignService
        }
        guard service == "officestra-backend", health.apiVersion == 1 else {
            return .foreignService
        }
        guard health.database?.ok == true else {
            return .unavailable
        }
        guard health.ok else {
            return .unavailable
        }
        guard let workdir = health.workdir else {
            return .unavailable
        }
        let actual = URL(fileURLWithPath: workdir)
            .standardizedFileURL
            .resolvingSymlinksInPath()
            .path
        let expected = URL(fileURLWithPath: expectedWorkdir)
            .standardizedFileURL
            .resolvingSymlinksInPath()
            .path
        guard actual == expected else {
            return .wrongWorkspace(actual)
        }
        if let expectedReleaseID,
           health.releaseID != expectedReleaseID
        {
            return .versionMismatch(health.releaseID)
        }
        return .ready
    }

    static func fetch(at url: URL) async -> OfficeBackendHealth? {
        var request = URLRequest(url: url)
        request.timeoutInterval = 2
        do {
            let (data, response) = try await URLSession.shared.data(
                for: request
            )
            guard response is HTTPURLResponse else {
                return nil
            }
            return (try? JSONDecoder().decode(
                OfficeBackendHealth.self,
                from: data
            )) ?? OfficeBackendHealth(
                ok: false,
                service: "unexpected-http-service",
                apiVersion: nil,
                pid: nil,
                releaseID: nil,
                activeTurnCount: nil,
                acceptingJobs: nil,
                draining: nil,
                idle: nil,
                workdir: nil,
                database: nil
            )
        } catch {
            return nil
        }
    }

}

enum OfficeBackendReplacementError: LocalizedError {
    case drainUnavailable
    case drainRejected
    case drainTimedOut
    case ownershipUnknown
    case stopFailed(String)
    case stopDidNotComplete

    var errorDescription: String? {
        switch self {
        case .drainUnavailable:
            OfficeLocalization.string(
                "기존 백엔드가 안전 종료 요청에 응답하지 않아 바꾸지 않았습니다."
            )
        case .drainRejected:
            OfficeLocalization.string(
                "기존 백엔드가 새 업무 수락을 멈춘 상태를 확인할 수 없어 바꾸지 않았습니다."
            )
        case .drainTimedOut:
            OfficeLocalization.string(
                "기존 업무가 안전하게 끝나기를 기다리다 시간이 초과되어 백엔드를 바꾸지 않았습니다."
            )
        case .ownershipUnknown:
            OfficeLocalization.string(
                "4317 백엔드가 OFFICESTRA launchd 작업인지 확인할 수 없어 중지하지 않았습니다."
            )
        case .stopFailed(let message):
            OfficeLocalization.format("백엔드 중지에 실패했습니다: %@", message)
        case .stopDidNotComplete:
            OfficeLocalization.string(
                "기존 백엔드가 완전히 종료되지 않아 새 백엔드를 시작하지 않았습니다."
            )
        }
    }
}

struct OfficeBackendDrainState: Decodable, Equatable, Sendable {
    let ok: Bool?
    let acceptingJobs: Bool?
    let draining: Bool?
    let activeTurnCount: Int?
    let idle: Bool?

    var confirmsDrainStarted: Bool {
        ok == true && acceptingJobs == false && draining == true
    }

    var confirmsIdle: Bool {
        guard confirmsDrainStarted else {
            return false
        }
        switch (idle, activeTurnCount) {
        case let (.some(idle), .some(activeTurnCount)):
            return idle && activeTurnCount == 0
        case let (.some(idle), .none):
            return idle
        case let (.none, .some(activeTurnCount)):
            return activeTurnCount == 0
        case (.none, .none):
            return false
        }
    }
}

enum OfficeBackendProcessPresence: Equatable, Sendable {
    case absent
    case present(jobPID: Int?, listenerPIDs: Set<Int>)
    case unknown
}

enum OfficeBackendReplacementSafety {
    private static let drainWaitAttempts = 240
    private static let stopWaitAttempts = 50
    static let requiredAbsentSamples = 3

    static func jobPID(from output: String) -> Int? {
        guard let expression = try? NSRegularExpression(
            pattern: "(?m)^\\s*pid = ([0-9]+)\\s*$"
        ) else {
            return nil
        }
        let fullRange = NSRange(output.startIndex..., in: output)
        guard let match = expression.firstMatch(
            in: output,
            range: fullRange
        ), let range = Range(match.range(at: 1), in: output) else {
            return nil
        }
        return Int(output[range])
    }

    static func listenerPIDs(from output: String) -> Set<Int> {
        Set(output.split(whereSeparator: \.isNewline).compactMap { line in
            Int(line.trimmingCharacters(in: .whitespacesAndNewlines))
        })
    }

    static func ownsListener(
        jobPID: Int?,
        listenerPIDs: Set<Int>,
        healthPID: Int?
    ) -> Bool {
        guard let jobPID, listenerPIDs == [jobPID] else {
            return false
        }
        return healthPID == nil || healthPID == jobPID
    }

    static func processPresence(
        job: OfficeProcessResult,
        listener: OfficeProcessResult
    ) -> OfficeBackendProcessPresence {
        let jobExists: Bool?
        if job.succeeded {
            jobExists = true
        } else if job.exitCode == 113 {
            // macOS launchctl은 등록되지 않은 job에 대해 EX_NOPERM(113)을
            // 반환한다. 다른 실패를 부재로 오인하면 기존 job을 덮어쓸 수 있다.
            jobExists = false
        } else {
            jobExists = nil
        }

        let listeners = listenerPIDs(from: listener.output)
        let listenerKnown = (listener.succeeded && !listeners.isEmpty)
            || (listener.exitCode == 1 && listeners.isEmpty)
        guard let jobExists, listenerKnown else {
            return .unknown
        }
        if !jobExists && listeners.isEmpty {
            return .absent
        }
        return .present(
            jobPID: jobExists ? jobPID(from: job.output) : nil,
            listenerPIDs: listeners
        )
    }

    static func nextConsecutiveAbsenceCount(
        previous: Int,
        presence: OfficeBackendProcessPresence
    ) -> Int {
        presence == .absent ? previous + 1 : 0
    }

    static func confirmsStopped(consecutiveAbsenceCount: Int) -> Bool {
        consecutiveAbsenceCount >= requiredAbsentSamples
    }

    static func inspectPresence(
        healthURL: URL,
        userID: uid_t = getuid()
    ) async -> OfficeBackendProcessPresence {
        let jobTarget = "gui/\(userID)/\(OfficeBackendLaunchConfiguration.jobLabel)"
        async let job = OfficeProcessRunner.run(
            executableURL: URL(fileURLWithPath: "/bin/launchctl"),
            arguments: ["print", jobTarget],
            timeout: 8
        )
        let port = healthURL.port ?? 4317
        async let listener = OfficeProcessRunner.run(
            executableURL: URL(fileURLWithPath: "/usr/sbin/lsof"),
            arguments: [
                "-nP", "-t", "-iTCP:\(port)", "-sTCP:LISTEN",
            ],
            timeout: 8
        )
        return await processPresence(job: job, listener: listener)
    }

    static func replaceIdleBackend(
        healthURL: URL,
        userID: uid_t = getuid()
    ) async throws {
        let jobTarget = "gui/\(userID)/\(OfficeBackendLaunchConfiguration.jobLabel)"
        let initialPresence = await inspectPresence(
            healthURL: healthURL,
            userID: userID
        )
        if initialPresence == .absent {
            return
        }
        guard case let .present(jobPID, listenerPIDs) = initialPresence else {
            throw OfficeBackendReplacementError.ownershipUnknown
        }

        // 실행 프로세스와 리스너가 없는 중지된 launchd 등록은
        // drain할 서버가 없으므로 라벨만 안전하게 정리한다.
        if jobPID == nil && listenerPIDs.isEmpty {
            try await bootoutAndConfirmStopped(
                jobTarget: jobTarget,
                healthURL: healthURL,
                userID: userID
            )
            return
        }

        let health = await OfficeBackendHealthProbe.fetch(at: healthURL)
        guard ownsListener(
            jobPID: jobPID,
            listenerPIDs: listenerPIDs,
            healthPID: health?.pid
        ), let ownedJobPID = jobPID else {
            throw OfficeBackendReplacementError.ownershipUnknown
        }

        var shouldCancelDrain = false
        do {
            let drain = try await requestDrain(
                healthURL: healthURL,
                method: "POST"
            )
            guard drain.confirmsDrainStarted else {
                throw OfficeBackendReplacementError.drainRejected
            }
            shouldCancelDrain = true
            if !drain.confirmsIdle {
                try await waitUntilDrained(
                    healthURL: healthURL,
                    expectedPID: ownedJobPID
                )
            }

            let finalHealth = await OfficeBackendHealthProbe.fetch(at: healthURL)
            let finalPresence = await inspectPresence(
                healthURL: healthURL,
                userID: userID
            )
            guard let finalHealth,
                  finalHealth.pid == ownedJobPID,
                  drainState(from: finalHealth).confirmsIdle,
                  finalPresence == .present(
                    jobPID: ownedJobPID,
                    listenerPIDs: [ownedJobPID]
                  )
            else {
                throw OfficeBackendReplacementError.ownershipUnknown
            }

            try await bootoutAndConfirmStopped(
                jobTarget: jobTarget,
                healthURL: healthURL,
                userID: userID
            )
            shouldCancelDrain = false
        } catch {
            if shouldCancelDrain {
                _ = try? await requestDrain(
                    healthURL: healthURL,
                    method: "DELETE"
                )
            }
            throw error
        }
    }

    private static func requestDrain(
        healthURL: URL,
        method: String
    ) async throws -> OfficeBackendDrainState {
        let endpoint = healthURL.deletingLastPathComponent()
            .appending(path: "api/maintenance/drain")
        var request = URLRequest(url: endpoint)
        request.httpMethod = method
        request.timeoutInterval = 5
        request.setValue(
            "application/json",
            forHTTPHeaderField: "Content-Type"
        )
        request.httpBody = Data("{}".utf8)
        do {
            let (data, response) = try await URLSession.shared.data(for: request)
            guard (response as? HTTPURLResponse)?.statusCode == 200 else {
                throw OfficeBackendReplacementError.drainUnavailable
            }
            return try JSONDecoder().decode(
                OfficeBackendDrainState.self,
                from: data
            )
        } catch let error as OfficeBackendReplacementError {
            throw error
        } catch {
            throw OfficeBackendReplacementError.drainUnavailable
        }
    }

    private static func drainState(
        from health: OfficeBackendHealth
    ) -> OfficeBackendDrainState {
        OfficeBackendDrainState(
            ok: health.ok,
            acceptingJobs: health.acceptingJobs,
            draining: health.draining,
            activeTurnCount: health.activeTurnCount,
            idle: health.idle
        )
    }

    private static func waitUntilDrained(
        healthURL: URL,
        expectedPID: Int
    ) async throws {
        for _ in 0..<drainWaitAttempts {
            try Task.checkCancellation()
            if let health = await OfficeBackendHealthProbe.fetch(at: healthURL),
               health.pid == expectedPID,
               drainState(from: health).confirmsIdle
            {
                return
            }
            try await Task.sleep(for: .milliseconds(500))
        }
        throw OfficeBackendReplacementError.drainTimedOut
    }

    private static func bootoutAndConfirmStopped(
        jobTarget: String,
        healthURL: URL,
        userID: uid_t
    ) async throws {
        let stopped = await OfficeProcessRunner.run(
            executableURL: URL(fileURLWithPath: "/bin/launchctl"),
            arguments: ["bootout", jobTarget],
            timeout: 8
        )
        guard stopped.succeeded else {
            throw OfficeBackendReplacementError.stopFailed(
                OfficeSetupAssistant.safeDiagnostic(stopped.output)
            )
        }

        var consecutiveAbsenceCount = 0
        for _ in 0..<stopWaitAttempts {
            try Task.checkCancellation()
            try await Task.sleep(for: .milliseconds(100))
            consecutiveAbsenceCount = nextConsecutiveAbsenceCount(
                previous: consecutiveAbsenceCount,
                presence: await inspectPresence(
                    healthURL: healthURL,
                    userID: userID
                )
            )
            if confirmsStopped(
                consecutiveAbsenceCount: consecutiveAbsenceCount
            ) {
                return
            }
        }
        throw OfficeBackendReplacementError.stopDidNotComplete
    }
}

enum OfficeCharacterBackendCompatibility {
    static func unavailableBackends(
        in characters: [StoredCharacterProfile],
        authenticatedBackends: Set<AgentBackend>
    ) -> Set<AgentBackend> {
        Set(characters.map(\.backend)).subtracting(authenticatedBackends)
    }
}

enum OfficeDockerProjectSelection: Equatable, Sendable {
    case current
    case legacy
    case legacyConfirmationRequired
    case projectChoiceRequired

    var projectName: String? {
        switch self {
        case .current:
            OfficeDockerProject.currentName
        case .legacy:
            OfficeDockerProject.legacyName
        case .legacyConfirmationRequired, .projectChoiceRequired:
            nil
        }
    }
}

enum OfficeDockerProjectPreference: String, Equatable, Sendable {
    case current
    case legacy

    static let defaultsKey = "officeDockerProjectPreference"
    private static let legacyDefaultsKey =
        "officeUseConfirmedLegacyDockerProject"

    static func load(from defaults: UserDefaults) -> Self? {
        if let value = defaults.string(forKey: defaultsKey),
           let preference = Self(rawValue: value)
        {
            return preference
        }
        return defaults.bool(forKey: legacyDefaultsKey) ? .legacy : nil
    }

    func save(to defaults: UserDefaults) {
        defaults.set(rawValue, forKey: Self.defaultsKey)
        defaults.removeObject(forKey: Self.legacyDefaultsKey)
    }
}

enum OfficeDockerProject {
    static let currentName = "com-neo-officestra"
    static let currentVolumeName =
        "com-neo-officestra_office-postgres-data"
    static let legacyName = "infra"
    static let legacyVolumeName = "infra_office-postgres-data"

    static func selection(
        currentVolumeExists: Bool,
        legacyVolumeExists: Bool,
        preference: OfficeDockerProjectPreference?
    ) -> OfficeDockerProjectSelection {
        if currentVolumeExists && legacyVolumeExists {
            switch preference {
            case .current:
                return .current
            case .legacy:
                return .legacy
            case nil:
                return .projectChoiceRequired
            }
        }
        if currentVolumeExists || !legacyVolumeExists {
            return .current
        }
        return preference == .legacy ? .legacy : .legacyConfirmationRequired
    }

    static func selection(
        docker: URL,
        environment: [String: String],
        preference: OfficeDockerProjectPreference?
    ) async -> OfficeDockerProjectSelection {
        async let current = OfficeProcessRunner.run(
            executableURL: docker,
            arguments: ["volume", "inspect", currentVolumeName],
            environment: environment,
            timeout: 8
        )
        async let legacy = OfficeProcessRunner.run(
            executableURL: docker,
            arguments: ["volume", "inspect", legacyVolumeName],
            environment: environment,
            timeout: 8
        )
        let (currentResult, legacyResult) = await (current, legacy)
        return selection(
            currentVolumeExists: currentResult.succeeded,
            legacyVolumeExists: legacyResult.succeeded,
            preference: preference
        )
    }
}

enum OfficeRuntimeConfigurationWriter {
    static func write(
        workdir: String,
        availableBackends: Set<AgentBackend>,
        executablePaths: [AgentBackend: String],
        destinationURL: URL,
        fileManager: FileManager = .default
    ) throws {
        let configuration = try CharacterConfigurationAsset.load()
            .preparingForRuntime(
                workdir: workdir,
                availableBackends: availableBackends,
                executablePaths: executablePaths
            )
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        let data = try encoder.encode(configuration)
        let destinationDirectory = destinationURL.deletingLastPathComponent()
        try fileManager.createDirectory(
            at: destinationDirectory,
            withIntermediateDirectories: true
        )
        try fileManager.setAttributes(
            [.posixPermissions: 0o700],
            ofItemAtPath: destinationDirectory.path
        )
        try data.write(to: destinationURL, options: .atomic)
        try fileManager.setAttributes(
            [.posixPermissions: 0o600],
            ofItemAtPath: destinationURL.path
        )
    }
}

enum OfficeSetupAssistant {
    typealias Progress = @Sendable (String) async -> Void

    static func readyResultForReleaseMismatch(
        connection: OfficeBackendConnection,
        snapshot sourceSnapshot: OfficeSetupSnapshot,
        availableBackends: Set<AgentBackend>,
        executablePaths: [AgentBackend: String]
    ) -> OfficeSetupResult? {
        guard case .versionMismatch(let actual) = connection else {
            return nil
        }

        var snapshot = sourceSnapshot
        snapshot.docker = .ready("기존 로컬 데이터 사용")
        snapshot.database = .ready("연결됨")
        snapshot.backendReplacement = .differentRelease(actual)
        snapshot.backend = .ready(
            actual == nil
                ? "릴리스 정보가 없는 백엔드에 연결됐습니다. 앱은 계속 사용할 수 있습니다."
                : "다른 릴리스의 백엔드에 연결됐습니다. 앱은 계속 사용할 수 있습니다."
        )
        return .ready(
            snapshot: snapshot,
            availableBackends: availableBackends,
            executablePaths: executablePaths
        )
    }

    static func prepare(
        workdir: String,
        healthURL: URL,
        resourceURL: URL,
        developmentRoot: URL? = nil,
        dockerProjectPreference: OfficeDockerProjectPreference? = nil,
        releaseID: String = OfficeAppReleaseIdentity.current(),
        progress: @escaping Progress
    ) async -> OfficeSetupResult {
        var snapshot = OfficeSetupSnapshot(workspace: workdir)
        let layout = OfficeRuntimeLayout.resolve(
            resourceURL: resourceURL,
            developmentRoot: developmentRoot
        )

        await progress("앱 런타임을 확인하는 중")
        do {
            try layout.validate()
            let logDirectory = layout.standardOutputLog
                .deletingLastPathComponent()
            try FileManager.default.createDirectory(
                at: logDirectory,
                withIntermediateDirectories: true
            )
            try FileManager.default.setAttributes(
                [.posixPermissions: 0o700],
                ofItemAtPath: logDirectory.path
            )
            snapshot.runtime = .ready("내장 Node와 백엔드")
        } catch {
            snapshot.runtime = .failed(error.localizedDescription)
            return .failed(
                snapshot: snapshot,
                message: error.localizedDescription
            )
        }

        let environment = processEnvironment(layout: layout)
        await progress("AI CLI 로그인을 확인하는 중")
        async let codex = OfficeCLIInspector.inspect(
            backend: .codex,
            environment: environment
        )
        async let claude = OfficeCLIInspector.inspect(
            backend: .claude,
            environment: environment
        )
        async let antigravity = OfficeCLIInspector.inspect(
            backend: .antigravity,
            environment: environment
        )
        let (codexProbe, claudeProbe, antigravityProbe) = await (
            codex,
            claude,
            antigravity
        )
        snapshot.codex = OfficeCLIInspector.check(for: codexProbe)
        snapshot.claude = OfficeCLIInspector.check(for: claudeProbe)
        snapshot.antigravity = OfficeCLIInspector.check(
            for: antigravityProbe
        )

        let probes = [codexProbe, claudeProbe, antigravityProbe]
        let authenticated = Set(
            probes.filter(\.authenticated).map(\.backend)
        )
        let executablePaths = authenticatedExecutablePaths(from: probes)
        guard !authenticated.isEmpty else {
            snapshot.backend = .actionRequired(
                "AI CLI 하나 이상에 로그인해야 합니다."
            )
            return .needsAction(snapshot)
        }

        do {
            try OfficeRuntimeConfigurationWriter.write(
                workdir: workdir,
                availableBackends: authenticated,
                executablePaths: executablePaths,
                destinationURL: layout.runtimeConfiguration
            )
        } catch {
            snapshot.runtime = .failed("실행 설정을 저장하지 못했습니다.")
            return .failed(
                snapshot: snapshot,
                message: error.localizedDescription
            )
        }

        await progress("기존 백엔드 연결을 확인하는 중")
        let initialHealth = await OfficeBackendHealthProbe.fetch(at: healthURL)
        let initialConnection = OfficeBackendHealthProbe.classify(
            initialHealth,
            expectedWorkdir: workdir,
            expectedReleaseID: releaseID
        )
        if let result = readyResultForReleaseMismatch(
            connection: initialConnection,
            snapshot: snapshot,
            availableBackends: authenticated,
            executablePaths: executablePaths
        ) {
            return result
        }
        switch initialConnection {
        case .ready:
            if let issue = await readyStateIssue(
                healthURL: healthURL,
                authenticatedBackends: authenticated,
                executablePaths: executablePaths
            ) {
                if issue.allowsProviderRemap, authenticated.count == 1 {
                    snapshot.providerRemapTarget = authenticated.first
                }
                snapshot.backend = .actionRequired(issue.message)
                return .needsAction(snapshot)
            }
            snapshot.docker = .ready("기존 로컬 데이터 사용")
            snapshot.database = .ready("연결됨")
            snapshot.backend = .ready("현재 프로젝트에 연결됨")
            return .ready(
                snapshot: snapshot,
                availableBackends: authenticated,
                executablePaths: executablePaths
            )
        case .legacy:
            snapshot.backendReplacement = .legacy
            snapshot.backend = .actionRequired(
                OfficeLocalization.string(
                    "구형 OFFICESTRA 백엔드가 4317을 사용 중입니다. 기존 업무를 마친 뒤 백엔드를 종료하고 다시 확인하세요."
                )
            )
            return .needsAction(snapshot)
        case .wrongWorkspace(let actual):
            snapshot.backendReplacement = .wrongWorkspace(actual)
            snapshot.backend = .actionRequired(
                OfficeLocalization.format("다른 프로젝트에 연결돼 있습니다: %@", actual)
            )
            return .needsAction(snapshot)
        case .versionMismatch:
            preconditionFailure("릴리스 불일치는 비차단 준비 결과로 처리해야 합니다.")
        case .foreignService:
            snapshot.backend = .actionRequired(
                "4317 포트를 다른 프로그램이 사용하고 있습니다."
            )
            return .needsAction(snapshot)
        case .unavailable:
            break
        }

        guard let docker = OfficeToolLocator.docker() else {
            snapshot.docker = .actionRequired("Docker Desktop 설치가 필요합니다.")
            snapshot.database = .actionRequired("Docker 준비 후 시작됩니다.")
            snapshot.backend = .actionRequired("로컬 DB 준비를 기다립니다.")
            return .needsAction(snapshot)
        }

        await progress("Docker 실행 상태를 확인하는 중")
        let dockerInfo = await OfficeProcessRunner.run(
            executableURL: docker,
            arguments: ["info"],
            environment: environment,
            timeout: 12
        )
        guard dockerInfo.succeeded else {
            snapshot.docker = .actionRequired("Docker Desktop을 실행하세요.")
            snapshot.database = .actionRequired("Docker 준비 후 시작됩니다.")
            snapshot.backend = .actionRequired("로컬 DB 준비를 기다립니다.")
            return .needsAction(snapshot)
        }
        snapshot.docker = .ready("실행 중")

        await progress("로컬 PostgreSQL을 준비하는 중")
        snapshot.database = .checking("pgvector PostgreSQL 시작 중")
        let dockerSelection = await OfficeDockerProject.selection(
            docker: docker,
            environment: environment,
            preference: dockerProjectPreference
        )
        guard let dockerProject = dockerSelection.projectName else {
            switch dockerSelection {
            case .legacyConfirmationRequired:
                snapshot.dockerDataSelection = .legacyOnly
                snapshot.database = .actionRequired(
                    OfficeLocalization.string(
                        "이름이 겹칠 수 있는 구형 로컬 볼륨이 발견됐습니다. 예전 OFFICESTRA 데이터가 맞는 경우에만 기존 데이터 사용을 선택하세요."
                    )
                )
            case .projectChoiceRequired:
                snapshot.dockerDataSelection = .currentAndLegacy
                snapshot.database = .actionRequired(
                    OfficeLocalization.string(
                        "현재 형식과 구형 형식의 OFFICESTRA 데이터가 모두 발견됐습니다. 사용할 데이터를 선택하세요. 어느 쪽도 삭제되지 않습니다."
                    )
                )
            case .current, .legacy:
                preconditionFailure("선택된 Docker 프로젝트에는 이름이 있어야 합니다.")
            }
            snapshot.backend = .actionRequired(
                "사용할 로컬 데이터를 먼저 확인하세요."
            )
            return .needsAction(snapshot)
        }
        let database = await OfficeProcessRunner.run(
            executableURL: docker,
            arguments: [
                "compose",
                "--project-name", dockerProject,
                "-f", layout.composeFile.path,
                "up", "-d", "--wait", "--wait-timeout", "120",
            ],
            environment: environment,
            timeout: 150
        )
        guard database.succeeded else {
            let message = safeDiagnostic(database.output)
            snapshot.database = .failed(
                message.isEmpty ? "PostgreSQL을 시작하지 못했습니다." : message
            )
            snapshot.backend = .actionRequired("DB 오류를 먼저 해결하세요.")
            return .needsAction(snapshot)
        }
        snapshot.database = .ready("데이터 보존 볼륨 준비 완료")

        // DB만 잠시 내려갔던 기존 백엔드가 회복됐으면 그 프로세스를
        // 무조건 bootout하지 않는다. 소유권·업무 폴더·릴리스를 다시 판정한다.
        let recoveredHealth = await OfficeBackendHealthProbe.fetch(at: healthURL)
        let recoveredConnection = OfficeBackendHealthProbe.classify(
            recoveredHealth,
            expectedWorkdir: workdir,
            expectedReleaseID: releaseID
        )
        if let result = readyResultForReleaseMismatch(
            connection: recoveredConnection,
            snapshot: snapshot,
            availableBackends: authenticated,
            executablePaths: executablePaths
        ) {
            return result
        }
        switch recoveredConnection {
        case .ready:
            if let issue = await readyStateIssue(
                healthURL: healthURL,
                authenticatedBackends: authenticated,
                executablePaths: executablePaths
            ) {
                if issue.allowsProviderRemap, authenticated.count == 1 {
                    snapshot.providerRemapTarget = authenticated.first
                }
                snapshot.backend = .actionRequired(issue.message)
                return .needsAction(snapshot)
            }
            snapshot.backend = .ready("현재 프로젝트에 다시 연결됨")
            return .ready(
                snapshot: snapshot,
                availableBackends: authenticated,
                executablePaths: executablePaths
            )
        case .legacy:
            snapshot.backendReplacement = .legacy
            snapshot.backend = .actionRequired(
                "구형 OFFICESTRA 백엔드가 회복됐습니다. 안전 확인 후 교체하세요."
            )
            return .needsAction(snapshot)
        case .wrongWorkspace(let actual):
            snapshot.backendReplacement = .wrongWorkspace(actual)
            snapshot.backend = .actionRequired(
                OfficeLocalization.format("다른 프로젝트 백엔드가 회복됐습니다: %@", actual)
            )
            return .needsAction(snapshot)
        case .versionMismatch:
            preconditionFailure("릴리스 불일치는 비차단 준비 결과로 처리해야 합니다.")
        case .foreignService:
            snapshot.backend = .actionRequired(
                "4317 포트를 다른 프로그램이 사용하고 있습니다."
            )
            return .needsAction(snapshot)
        case .unavailable:
            break
        }

        // 건강 확인이 실패했다는 이유만으로 기존 launchd job을 내리면
        // 실행 중인 업무를 끊을 수 있다. job 또는 4317 리스너가 남아 있거나
        // 확인 자체가 불확실하면 사용자가 명시적으로 안전 교체를 실행하게 한다.
        switch await OfficeBackendReplacementSafety.inspectPresence(
            healthURL: healthURL
        ) {
        case .absent:
            break
        case .present, .unknown:
            snapshot.backendReplacement = .unresponsive
            snapshot.backend = .actionRequired(
                OfficeLocalization.string(
                    "응답하지 않는 기존 백엔드가 남아 있을 수 있습니다. 안전 종료를 확인한 뒤 교체하세요."
                )
            )
            return .needsAction(snapshot)
        }

        await progress("OFFICESTRA 백엔드를 시작하는 중")
        let launchConfiguration = OfficeBackendLaunchConfiguration(
            workdir: URL(fileURLWithPath: workdir),
            backendDirectoryURL: layout.backendDirectory,
            healthURL: healthURL,
            runtimeConfigurationURL: layout.runtimeConfiguration,
            releaseID: releaseID,
            userID: getuid()
        )
        let start = OfficeBackendLaunchCommands.start(
            configuration: launchConfiguration,
            nodeExecutableURL: layout.nodeExecutable,
            rustExecutableURL: OfficeBackendRuntimeLocation.rustExecutable(
                backendDirectoryURL: layout.backendDirectory
            ),
            standardOutputURL: layout.standardOutputLog,
            standardErrorURL: layout.standardErrorLog,
            executableSearchPaths: executablePaths.values.map {
                URL(fileURLWithPath: $0).deletingLastPathComponent().path
            }
        )
        let started = await OfficeProcessRunner.run(
            executableURL: start.executableURL,
            arguments: start.arguments,
            environment: environment,
            timeout: 10
        )
        guard started.succeeded else {
            snapshot.backend = .failed(safeDiagnostic(started.output))
            return .needsAction(snapshot)
        }

        for _ in 0..<60 {
            try? await Task.sleep(for: .milliseconds(500))
            let health = await OfficeBackendHealthProbe.fetch(at: healthURL)
            let startedConnection = OfficeBackendHealthProbe.classify(
                health,
                expectedWorkdir: workdir,
                expectedReleaseID: releaseID
            )
            if let result = readyResultForReleaseMismatch(
                connection: startedConnection,
                snapshot: snapshot,
                availableBackends: authenticated,
                executablePaths: executablePaths
            ) {
                return result
            }
            switch startedConnection {
            case .ready:
                if let issue = await readyStateIssue(
                    healthURL: healthURL,
                    authenticatedBackends: authenticated,
                    executablePaths: executablePaths
                ) {
                    if issue.allowsProviderRemap, authenticated.count == 1 {
                        snapshot.providerRemapTarget = authenticated.first
                    }
                    snapshot.backend = .actionRequired(issue.message)
                    return .needsAction(snapshot)
                }
                snapshot.backend = .ready("4317 연결 완료")
                return .ready(
                    snapshot: snapshot,
                    availableBackends: authenticated,
                    executablePaths: executablePaths
                )
            case .unavailable:
                continue
            case .legacy:
                snapshot.backendReplacement = .legacy
                snapshot.backend = .actionRequired(
                    "구형 OFFICESTRA 백엔드가 4317을 사용 중입니다."
                )
                return .needsAction(snapshot)
            case .wrongWorkspace(let actual):
                snapshot.backendReplacement = .wrongWorkspace(actual)
                snapshot.backend = .actionRequired(
                    OfficeLocalization.format("다른 프로젝트에 연결돼 있습니다: %@", actual)
                )
                return .needsAction(snapshot)
            case .versionMismatch:
                preconditionFailure("릴리스 불일치는 비차단 준비 결과로 처리해야 합니다.")
            case .foreignService:
                snapshot.backend = .actionRequired(
                    "4317 포트를 다른 프로그램이 사용하고 있습니다."
                )
                return .needsAction(snapshot)
            }
        }
        snapshot.backend = .failed(
            "백엔드가 30초 안에 준비되지 않았습니다."
        )
        return .needsAction(snapshot)
    }

    static func processEnvironment(
        layout: OfficeRuntimeLayout,
        base: [String: String] = ProcessInfo.processInfo.environment
    ) -> [String: String] {
        var environment = base
        let paths = [
            layout.nodeExecutable.deletingLastPathComponent().path,
            FileManager.default.homeDirectoryForCurrentUser
                .appending(path: ".local/bin").path,
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/usr/bin",
            "/bin",
            "/usr/sbin",
            "/sbin",
        ] + String(base["PATH"] ?? "")
            .split(separator: ":")
            .map(String.init)
        environment["PATH"] = paths.reduce(into: [String]()) {
            if !$0.contains($1) {
                $0.append($1)
            }
        }.joined(separator: ":")
        return environment
    }

    static func safeDiagnostic(_ output: String) -> String {
        let sensitive = try? NSRegularExpression(
            pattern: "(?im)^.*(?:token|password|secret|authorization).*$"
        )
        let range = NSRange(output.startIndex..., in: output)
        let redacted = sensitive?.stringByReplacingMatches(
            in: output,
            range: range,
            withTemplate: NSRegularExpression.escapedTemplate(for: OfficeLocalization.string("[민감 정보가 포함된 줄 숨김]"))
        ) ?? output
        return String(redacted.suffix(2_000))
            .trimmingCharacters(in: .whitespacesAndNewlines)
    }

    static func remapExistingCharacters(
        healthURL: URL,
        to backend: AgentBackend
    ) async throws {
        let database = OfficeDatabaseClient(
            baseURL: healthURL.deletingLastPathComponent()
        )
        let characters = try await database.fetchCharacters()
        let updates = remappedCharacterSettings(
            characters,
            to: backend
        )
        guard !updates.isEmpty else {
            return
        }
        _ = try await database.updateSettings(updates)
    }

    static func remappedCharacterSettings(
        _ characters: [StoredCharacterProfile],
        to backend: AgentBackend
    ) -> [CharacterSettingsBulkUpdate] {
        characters.compactMap { character in
            guard let characterID = OfficeCharacter(rawValue: character.id) else {
                return nil
            }
            let effortOptions = backend.effortOptions(
                for: backend.defaultModel
            )
            let effort = effortOptions.contains(character.effort)
                ? character.effort
                : (effortOptions.contains("high")
                    ? "high"
                    : effortOptions[0])
            let settings = CharacterAgentSettings(
                backend: backend,
                model: backend.defaultModel,
                effort: effort,
                fastMode: character.fastMode
                    && backend.supportsFastMode(model: backend.defaultModel),
                permission: AgentPermission(cliValue: character.permission)
            )
            return CharacterSettingsBulkUpdate(
                character: characterID,
                settings: settings
            )
        }
    }

    static func authenticatedExecutablePaths(
        from probes: [OfficeCLIProbe]
    ) -> [AgentBackend: String] {
        Dictionary(
            uniqueKeysWithValues: probes.compactMap { probe in
                guard probe.authenticated, let executable = probe.executableURL else {
                    return nil
                }
                return (probe.backend, executable.path)
            }
        )
    }

    private static func readyStateIssue(
        healthURL: URL,
        authenticatedBackends: Set<AgentBackend>,
        executablePaths: [AgentBackend: String]
    ) async -> OfficeSetupReadyStateIssue? {
        let database = OfficeDatabaseClient(
            baseURL: healthURL.deletingLastPathComponent()
        )
        return await evaluateReadyState(
            synchronizeCLIPaths: {
                _ = try await database.synchronizeRuntimeCLIPaths(
                    executablePaths
                )
            },
            compatibilityError: {
                await compatibilityError(
                    database: database,
                    authenticatedBackends: authenticatedBackends
                )
            }
        )
    }

    static func evaluateReadyState(
        synchronizeCLIPaths: () async throws -> Void,
        compatibilityError: () async -> String?
    ) async -> OfficeSetupReadyStateIssue? {
        do {
            try await synchronizeCLIPaths()
        } catch {
            return OfficeSetupReadyStateIssue(
                kind: .cliPathSynchronization,
                message: OfficeLocalization.format(
                    "탐지한 AI CLI 실행 경로를 백엔드에 반영하지 못했습니다: %@",
                    error.localizedDescription
                )
            )
        }
        guard let message = await compatibilityError() else {
            return nil
        }
        return OfficeSetupReadyStateIssue(
            kind: .providerCompatibility,
            message: message
        )
    }

    private static func compatibilityError(
        database: OfficeDatabaseClient,
        authenticatedBackends: Set<AgentBackend>
    ) async -> String? {
        do {
            let characters = try await database.fetchCharacters()
            let unavailable = OfficeCharacterBackendCompatibility
                .unavailableBackends(
                    in: characters,
                    authenticatedBackends: authenticatedBackends
                )
            guard !unavailable.isEmpty else {
                return nil
            }
            let names = unavailable
                .map(\.title)
                .sorted()
                .joined(separator: ", ")
            return OfficeLocalization.format(
                "기존 직원 설정이 로그인되지 않은 CLI를 사용합니다: %@. 해당 CLI에 로그인한 뒤 다시 확인하세요.",
                names
            )
        } catch {
            return OfficeLocalization.string(
                "실행 중인 백엔드의 직원 설정을 확인하지 못했습니다. 로그를 확인한 뒤 다시 시도하세요."
            )
        }
    }
}

private enum OfficeSetupError: LocalizedError {
    case runtimeIncomplete
    case bundledNodeNotExecutable

    var errorDescription: String? {
        switch self {
        case .runtimeIncomplete:
            "앱에 포함된 실행 파일이 완전하지 않습니다. 앱을 다시 설치하세요."
        case .bundledNodeNotExecutable:
            "앱에 포함된 Node 실행 파일을 실행할 수 없습니다."
        }
    }
}
