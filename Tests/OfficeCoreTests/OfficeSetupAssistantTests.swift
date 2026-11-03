// 이 파일은 새 Mac 설치 도우미의 순수 판정과 백엔드 실행 명령을 검증한다.

import Foundation
import XCTest
@testable import OfficeCore
@testable import OfficeGame

final class OfficeSetupAssistantTests: XCTestCase {
    private var root: URL!

    override func setUpWithError() throws {
        root = FileManager.default.temporaryDirectory
            .appending(path: UUID().uuidString)
        try FileManager.default.createDirectory(
            at: root,
            withIntermediateDirectories: true
        )
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: root)
        root = nil
    }

    func testRuntimeLayoutKeepsBundledAndWritableFilesSeparate() {
        let resources = URL(
            fileURLWithPath:
                "/Applications/OFFICESTRA.app/Contents/Resources"
        )
        let support = root.appending(path: "Application Support/OFFICESTRA")

        let layout = OfficeRuntimeLayout.resolve(
            resourceURL: resources,
            supportDirectory: support
        )

        XCTAssertEqual(
            layout.runtimeRoot.path,
            "/Applications/OFFICESTRA.app/Contents/Resources/OFFICESTRARuntime"
        )
        XCTAssertEqual(
            layout.backendDirectory.path,
            "\(layout.runtimeRoot.path)/backend"
        )
        XCTAssertEqual(
            layout.nodeExecutable.path,
            "\(layout.runtimeRoot.path)/node/bin/node"
        )
        XCTAssertEqual(
            layout.composeFile.path,
            "\(layout.runtimeRoot.path)/infra/compose.yaml"
        )
        XCTAssertEqual(
            layout.bundledConfiguration.path,
            "\(resources.path)/OfficeLLM_OfficeCore.bundle/characters.json"
        )
        XCTAssertEqual(
            layout.runtimeConfiguration.path,
            "\(support.path)/runtime/characters.json"
        )
        XCTAssertEqual(
            layout.standardOutputLog.path,
            "\(support.path)/logs/backend.out.log"
        )
        XCTAssertEqual(
            layout.standardErrorLog.path,
            "\(support.path)/logs/backend.err.log"
        )
    }

    func testDevelopmentRuntimeUsesSourceBackendAndSystemNode() throws {
        let developmentRoot = root.appending(path: "OFFICESTRA")
        let node = root.appending(path: "tools/node")
        try createExecutable(at: node)

        let layout = OfficeRuntimeLayout.resolve(
            resourceURL: root.appending(path: "build/resources"),
            supportDirectory: root.appending(path: "support"),
            developmentRoot: developmentRoot,
            developmentNodeExecutable: node
        )

        XCTAssertEqual(layout.runtimeRoot, developmentRoot)
        XCTAssertEqual(
            layout.backendDirectory,
            developmentRoot.appending(path: "backend")
        )
        XCTAssertEqual(layout.nodeExecutable, node)
        XCTAssertEqual(
            layout.composeFile,
            developmentRoot.appending(path: "infra/compose.yaml")
        )
    }

    func testDevelopmentRuntimeLocatorWalksUpFromBuildDirectory() throws {
        let repository = root.appending(path: "repository")
        try createFile(at: repository.appending(path: "Package.swift"))
        try createFile(
            at: repository.appending(path: "scripts/build-app.sh")
        )
        try createFile(at: repository.appending(path: "infra/compose.yaml"))
        let buildDirectory = repository.appending(path: ".build/debug")
        try FileManager.default.createDirectory(
            at: buildDirectory,
            withIntermediateDirectories: true
        )

        XCTAssertEqual(
            OfficeDevelopmentRuntimeLocator.locate(
                startingAt: [buildDirectory]
            )?.path,
            repository.path
        )
    }

    func testRuntimeValidationRequiresRustAndNativeResultWithoutNode()
        throws
    {
        let layout = OfficeRuntimeLayout.resolve(
            resourceURL: root.appending(path: "Resources"),
            supportDirectory: root.appending(path: "Support")
        )
        let rust = layout.runtimeRoot.appending(path: "rust/bin/officestra-backend")
        let result = layout.runtimeRoot.appending(path: "rust/bin/officestra-result")
        try createFile(at: rust)
        try createExecutable(at: result)
        try createFile(at: layout.composeFile)
        try createFile(at: layout.bundledConfiguration)

        XCTAssertThrowsError(try layout.validate()) { error in
            XCTAssertEqual(
                error.localizedDescription,
                "앱에 포함된 실행 파일이 완전하지 않습니다. 앱을 다시 설치하세요."
            )
        }

        try makeExecutable(rust)
        XCTAssertNoThrow(try layout.validate())
        XCTAssertFalse(FileManager.default.fileExists(atPath: layout.nodeExecutable.path))

        try FileManager.default.removeItem(at: layout.composeFile)
        XCTAssertThrowsError(try layout.validate()) { error in
            XCTAssertEqual(
                error.localizedDescription,
                "앱에 포함된 실행 파일이 완전하지 않습니다. 앱을 다시 설치하세요."
            )
        }
    }

    func testRuntimeConfigurationIsPrivateAndUsesTheSelectedWorkspace() throws {
        let destination = root.appending(
            path: "Support/runtime/characters.json"
        )

        try OfficeRuntimeConfigurationWriter.write(
            workdir: "/Users/example/project",
            availableBackends: [.codex],
            executablePaths: [.codex: "/opt/local/bin/codex"],
            destinationURL: destination
        )

        let attributes = try FileManager.default.attributesOfItem(
            atPath: destination.path
        )
        let permissions = try XCTUnwrap(
            attributes[.posixPermissions] as? NSNumber
        )
        XCTAssertEqual(permissions.intValue & 0o777, 0o600)

        let configuration = try JSONDecoder().decode(
            OfficeAgentConfiguration.self,
            from: Data(contentsOf: destination)
        )
        XCTAssertEqual(configuration.workdir, "/Users/example/project")
        XCTAssertTrue(configuration.characters.allSatisfy {
            $0.backend == .codex
                && $0.executablePath == "/opt/local/bin/codex"
        })
    }

    func testBackendHealthClassificationDistinguishesEveryOwnershipState()
        throws
    {
        let workspace = root.appending(path: "workspace")
        try FileManager.default.createDirectory(
            at: workspace,
            withIntermediateDirectories: true
        )

        XCTAssertEqual(
            OfficeBackendHealthProbe.classify(
                nil,
                expectedWorkdir: workspace.path
            ),
            .unavailable
        )
        XCTAssertEqual(
            OfficeBackendHealthProbe.classify(
                health(ok: false),
                expectedWorkdir: workspace.path
            ),
            .unavailable
        )
        XCTAssertEqual(
            OfficeBackendHealthProbe.classify(
                OfficeBackendHealth(
                    ok: true,
                    service: nil,
                    apiVersion: nil,
                    pid: nil,
                    releaseID: nil,
                    activeTurnCount: nil,
                    acceptingJobs: nil,
                    draining: nil,
                    idle: nil,
                    workdir: nil,
                    database: nil
                ),
                expectedWorkdir: workspace.path
            ),
            .legacy
        )
        XCTAssertEqual(
            OfficeBackendHealthProbe.classify(
                health(service: "another-service"),
                expectedWorkdir: workspace.path
            ),
            .foreignService
        )
        XCTAssertEqual(
            OfficeBackendHealthProbe.classify(
                health(ok: false, service: "another-service"),
                expectedWorkdir: workspace.path
            ),
            .foreignService
        )
        XCTAssertEqual(
            OfficeBackendHealthProbe.classify(
                health(apiVersion: 2),
                expectedWorkdir: workspace.path
            ),
            .foreignService
        )
        XCTAssertEqual(
            OfficeBackendHealthProbe.classify(
                health(workdir: nil),
                expectedWorkdir: workspace.path
            ),
            .unavailable
        )
        XCTAssertEqual(
            OfficeBackendHealthProbe.classify(
                health(workdir: workspace.path),
                expectedWorkdir: workspace.path
            ),
            .ready
        )
        XCTAssertEqual(
            OfficeBackendHealthProbe.classify(
                health(
                    workdir: workspace.path,
                    releaseID: "old-release"
                ),
                expectedWorkdir: workspace.path,
                expectedReleaseID: "new-release"
            ),
            .versionMismatch("old-release")
        )
        XCTAssertEqual(
            OfficeBackendHealthProbe.classify(
                health(
                    workdir: workspace.path,
                    databaseOK: false
                ),
                expectedWorkdir: workspace.path
            ),
            .unavailable
        )

        let other = root.appending(path: "other")
        XCTAssertEqual(
            OfficeBackendHealthProbe.classify(
                health(workdir: other.path),
                expectedWorkdir: workspace.path
            ),
            .wrongWorkspace(other.path)
        )
    }

    func testReleaseMismatchDoesNotBlockAppLaunch() {
        let availableBackends: Set<AgentBackend> = [.codex]
        let executablePaths: [AgentBackend: String] = [
            .codex: "/usr/local/bin/codex",
        ]
        guard let result = OfficeSetupAssistant.readyResultForReleaseMismatch(
            connection: .versionMismatch("another-release"),
            snapshot: OfficeSetupSnapshot(workspace: "/tmp/project"),
            availableBackends: availableBackends,
            executablePaths: executablePaths
        ) else {
            return XCTFail("릴리스 불일치는 비차단 준비 결과여야 합니다.")
        }
        guard case let .ready(
            snapshot,
            resultBackends,
            resultExecutablePaths
        ) = result else {
            return XCTFail("릴리스 불일치는 앱 시작을 막으면 안 됩니다.")
        }

        XCTAssertEqual(resultBackends, availableBackends)
        XCTAssertEqual(resultExecutablePaths, executablePaths)
        XCTAssertTrue(snapshot.docker.isReady)
        XCTAssertTrue(snapshot.database.isReady)
        XCTAssertTrue(snapshot.backend.isReady)
        XCTAssertEqual(
            snapshot.backendReplacement,
            .differentRelease("another-release")
        )
        guard let unversionedResult = OfficeSetupAssistant
            .readyResultForReleaseMismatch(
                connection: .versionMismatch(nil),
                snapshot: snapshot,
                availableBackends: availableBackends,
                executablePaths: executablePaths
            ) else {
            return XCTFail("릴리스 정보가 없어도 앱 시작을 막으면 안 됩니다.")
        }
        guard case let .ready(unversionedSnapshot, _, _) = unversionedResult else {
            return XCTFail("릴리스 정보가 없는 백엔드도 준비 완료여야 합니다.")
        }
        XCTAssertEqual(
            unversionedSnapshot.backendReplacement,
            .differentRelease(nil)
        )

        for blockedConnection: OfficeBackendConnection in [
            .ready,
            .unavailable,
            .legacy,
            .wrongWorkspace("/tmp/other"),
            .foreignService,
        ] {
            XCTAssertNil(
                OfficeSetupAssistant.readyResultForReleaseMismatch(
                    connection: blockedConnection,
                    snapshot: snapshot,
                    availableBackends: availableBackends,
                    executablePaths: executablePaths
                )
            )
        }
    }

    @MainActor
    func testReleaseMismatchReadySnapshotKeepsNonblockingNotice() {
        var snapshot = OfficeSetupSnapshot(workspace: "/tmp/project")
        snapshot.backend = .ready(
            "다른 릴리스의 백엔드에 연결됐습니다. 앱은 계속 사용할 수 있습니다."
        )
        snapshot.backendReplacement = .differentRelease(nil)

        let notice = OfficeLaunchCoordinator.compatibilityNotice(
            from: snapshot
        )

        XCTAssertEqual(notice?.replacement, .differentRelease(nil))
        XCTAssertEqual(
            notice?.message,
            "다른 릴리스의 백엔드에 연결됐습니다. 앱은 계속 사용할 수 있습니다."
        )
        XCTAssertFalse(notice?.isReplacing ?? true)
        XCTAssertNil(notice?.errorMessage)
    }

    func testBackendReplacementRequiresExactLaunchdListenerOwnership() {
        let launchctl = """
        gui/501/com.neo.office-backend-4317 = {
            state = running
            pid = 43170
        }
        """
        XCTAssertEqual(
            OfficeBackendReplacementSafety.jobPID(from: launchctl),
            43_170
        )
        XCTAssertEqual(
            OfficeBackendReplacementSafety.listenerPIDs(
                from: "43170\n43170\n"
            ),
            [43_170]
        )
        XCTAssertTrue(
            OfficeBackendReplacementSafety.ownsListener(
                jobPID: 43_170,
                listenerPIDs: [43_170],
                healthPID: 43_170
            )
        )
        XCTAssertFalse(
            OfficeBackendReplacementSafety.ownsListener(
                jobPID: 43_170,
                listenerPIDs: [43_170, 99],
                healthPID: 43_170
            )
        )
        XCTAssertFalse(
            OfficeBackendReplacementSafety.ownsListener(
                jobPID: 43_170,
                listenerPIDs: [43_170],
                healthPID: 99
            )
        )
    }

    func testBackendDrainRequiresExplicitStoppedAcceptanceAndIdleState() {
        let drainingWithWork = OfficeBackendDrainState(
            ok: true,
            acceptingJobs: false,
            draining: true,
            activeTurnCount: 2,
            idle: false
        )
        XCTAssertTrue(drainingWithWork.confirmsDrainStarted)
        XCTAssertFalse(drainingWithWork.confirmsIdle)

        XCTAssertFalse(
            OfficeBackendDrainState(
                ok: true,
                acceptingJobs: false,
                draining: true,
                activeTurnCount: 0,
                idle: false
            ).confirmsIdle
        )
        XCTAssertTrue(
            OfficeBackendDrainState(
                ok: true,
                acceptingJobs: false,
                draining: true,
                activeTurnCount: 0,
                idle: true
            ).confirmsIdle
        )
        XCTAssertTrue(
            OfficeBackendDrainState(
                ok: true,
                acceptingJobs: false,
                draining: true,
                activeTurnCount: nil,
                idle: true
            ).confirmsIdle
        )
        XCTAssertFalse(
            OfficeBackendDrainState(
                ok: true,
                acceptingJobs: false,
                draining: true,
                activeTurnCount: 2,
                idle: true
            ).confirmsIdle
        )
        XCTAssertFalse(
            OfficeBackendDrainState(
                ok: true,
                acceptingJobs: nil,
                draining: nil,
                activeTurnCount: 0,
                idle: true
            ).confirmsIdle
        )
    }

    func testBackendHealthDrainFieldsDecodeWithoutBreakingLegacyPayloads()
        throws
    {
        let decoder = JSONDecoder()
        let legacy = try decoder.decode(
            OfficeBackendHealth.self,
            from: Data(#"{"ok":true}"#.utf8)
        )
        XCTAssertNil(legacy.acceptingJobs)
        XCTAssertNil(legacy.draining)
        XCTAssertNil(legacy.idle)

        let draining = try decoder.decode(
            OfficeBackendHealth.self,
            from: Data(
                #"{"ok":true,"acceptingJobs":false,"draining":true,"activeTurnCount":0,"idle":true}"#.utf8
            )
        )
        XCTAssertEqual(draining.acceptingJobs, false)
        XCTAssertEqual(draining.draining, true)
        XCTAssertEqual(draining.activeTurnCount, 0)
        XCTAssertEqual(draining.idle, true)
    }

    func testBackendPresenceRequiresBothLaunchdJobAndListenerToBeAbsent() {
        let launchdMissing = OfficeProcessResult(exitCode: 113, output: "")
        let noListener = OfficeProcessResult(exitCode: 1, output: "")
        XCTAssertEqual(
            OfficeBackendReplacementSafety.processPresence(
                job: launchdMissing,
                listener: noListener
            ),
            .absent
        )

        let launchdRunning = OfficeProcessResult(
            exitCode: 0,
            output: "pid = 43170"
        )
        let listener = OfficeProcessResult(exitCode: 0, output: "43170")
        XCTAssertEqual(
            OfficeBackendReplacementSafety.processPresence(
                job: launchdRunning,
                listener: listener
            ),
            .present(jobPID: 43_170, listenerPIDs: [43_170])
        )
        XCTAssertEqual(
            OfficeBackendReplacementSafety.processPresence(
                job: launchdMissing,
                listener: listener
            ),
            .present(jobPID: nil, listenerPIDs: [43_170])
        )
        XCTAssertEqual(
            OfficeBackendReplacementSafety.processPresence(
                job: OfficeProcessResult(exitCode: 0, output: "state = exited"),
                listener: noListener
            ),
            .present(jobPID: nil, listenerPIDs: [])
        )
    }

    func testBackendPresenceTreatsAmbiguousProcessChecksAsUnknown() {
        let noListener = OfficeProcessResult(exitCode: 1, output: "")
        XCTAssertEqual(
            OfficeBackendReplacementSafety.processPresence(
                job: OfficeProcessResult(exitCode: 1, output: "permission denied"),
                listener: noListener
            ),
            .unknown
        )
        XCTAssertEqual(
            OfficeBackendReplacementSafety.processPresence(
                job: OfficeProcessResult(exitCode: 113, output: ""),
                listener: OfficeProcessResult(exitCode: -2, output: "timeout")
            ),
            .unknown
        )
        XCTAssertEqual(
            OfficeBackendReplacementSafety.processPresence(
                job: OfficeProcessResult(exitCode: 113, output: ""),
                listener: OfficeProcessResult(exitCode: 0, output: "")
            ),
            .unknown
        )
    }

    func testBackendStopRequiresThreeConsecutiveAbsentSamples() {
        var count = 0
        count = OfficeBackendReplacementSafety.nextConsecutiveAbsenceCount(
            previous: count,
            presence: .absent
        )
        XCTAssertFalse(
            OfficeBackendReplacementSafety.confirmsStopped(
                consecutiveAbsenceCount: count
            )
        )
        count = OfficeBackendReplacementSafety.nextConsecutiveAbsenceCount(
            previous: count,
            presence: .absent
        )
        XCTAssertFalse(
            OfficeBackendReplacementSafety.confirmsStopped(
                consecutiveAbsenceCount: count
            )
        )
        count = OfficeBackendReplacementSafety.nextConsecutiveAbsenceCount(
            previous: count,
            presence: .present(jobPID: 43_170, listenerPIDs: [43_170])
        )
        XCTAssertEqual(count, 0)

        for _ in 0..<OfficeBackendReplacementSafety.requiredAbsentSamples {
            count = OfficeBackendReplacementSafety.nextConsecutiveAbsenceCount(
                previous: count,
                presence: .absent
            )
        }
        XCTAssertTrue(
            OfficeBackendReplacementSafety.confirmsStopped(
                consecutiveAbsenceCount: count
            )
        )
    }

    func testBackendHealthTreatsSymlinkedWorkspaceAsTheSameDirectory() throws {
        let workspace = root.appending(path: "workspace")
        let alias = root.appending(path: "workspace-alias")
        try FileManager.default.createDirectory(
            at: workspace,
            withIntermediateDirectories: true
        )
        try FileManager.default.createSymbolicLink(
            at: alias,
            withDestinationURL: workspace
        )

        XCTAssertEqual(
            OfficeBackendHealthProbe.classify(
                health(workdir: workspace.path),
                expectedWorkdir: alias.path
            ),
            .ready
        )
    }

    func testExistingCharacterSettingsRequireEveryConfiguredBackend() {
        let characters = [
            storedCharacter(id: "boss", backend: .codex),
            storedCharacter(id: "left-man", backend: .claude),
        ]

        XCTAssertEqual(
            OfficeCharacterBackendCompatibility.unavailableBackends(
                in: characters,
                authenticatedBackends: [.codex]
            ),
            [.claude]
        )
        XCTAssertTrue(
            OfficeCharacterBackendCompatibility.unavailableBackends(
                in: characters,
                authenticatedBackends: [.codex, .claude]
            ).isEmpty
        )
    }

    func testCharacterRemapBuildsOneCompleteBatchAndSkipsUnknownRows() {
        let characters = [
            storedCharacter(
                id: "boss",
                backend: .codex,
                effort: "ultra",
                fastMode: true,
                permission: "danger-full-access"
            ),
            storedCharacter(
                id: "left-man",
                backend: .claude,
                effort: "max",
                permission: "plan"
            ),
            storedCharacter(
                id: "future-character",
                backend: .codex,
                effort: "xhigh"
            ),
        ]

        let updates = OfficeSetupAssistant.remappedCharacterSettings(
            characters,
            to: .claude
        )

        XCTAssertEqual(updates.map(\.characterId), ["boss", "left-man"])
        XCTAssertEqual(
            updates[0],
            CharacterSettingsBulkUpdate(
                character: .boss,
                settings: CharacterAgentSettings(
                    backend: .claude,
                    model: AgentBackend.claude.defaultModel,
                    effort: "high",
                    fastMode: true,
                    permission: .fullAccess
                )
            )
        )
        XCTAssertEqual(updates[1].effort, "max")
        XCTAssertEqual(updates[1].permission, "plan")
    }

    func testCharacterRemapDoesNotCreateAnEmptyUnknownOnlyBatch() {
        XCTAssertTrue(
            OfficeSetupAssistant.remappedCharacterSettings(
                [storedCharacter(id: "future-character", backend: .codex)],
                to: .claude
            ).isEmpty
        )
    }

    func testBulkSettingsRequestUsesAtomicEndpointAndCompletePayload() throws {
        let client = OfficeDatabaseClient(
            baseURL: URL(string: "http://127.0.0.1:4317")!
        )
        let update = CharacterSettingsBulkUpdate(
            character: .boss,
            settings: CharacterAgentSettings(
                backend: .claude,
                model: "claude-opus-5",
                effort: "max",
                fastMode: true,
                permission: .workspaceWrite
            )
        )

        let request = try XCTUnwrap(client.bulkSettingsRequest([update]))
        XCTAssertEqual(
            request.url?.absoluteString,
            "http://127.0.0.1:4317/api/characters/settings/bulk"
        )
        XCTAssertEqual(request.httpMethod, "PUT")
        XCTAssertEqual(
            request.value(forHTTPHeaderField: "Content-Type"),
            "application/json"
        )
        let body = try XCTUnwrap(request.httpBody)
        let object = try XCTUnwrap(
            JSONSerialization.jsonObject(with: body) as? [String: Any]
        )
        XCTAssertEqual(Set(object.keys), ["updates"])
        let updates = try XCTUnwrap(object["updates"] as? [[String: Any]])
        XCTAssertEqual(updates.count, 1)
        XCTAssertEqual(
            Set(updates[0].keys),
            [
                "characterId", "backend", "model", "effort", "fastMode",
                "permission",
            ]
        )
        XCTAssertEqual(updates[0]["characterId"] as? String, "boss")
        XCTAssertEqual(updates[0]["backend"] as? String, "claude")
        XCTAssertEqual(updates[0]["model"] as? String, "claude-opus-5")
        XCTAssertEqual(updates[0]["effort"] as? String, "max")
        XCTAssertEqual(updates[0]["fastMode"] as? Bool, true)
        XCTAssertEqual(updates[0]["permission"] as? String, "auto")
        XCTAssertNil(try client.bulkSettingsRequest([]))
    }

    func testEmptyBulkSettingsUpdateReturnsWithoutANetworkRequest() async throws {
        let client = OfficeDatabaseClient(
            baseURL: URL(string: "http://127.0.0.1:1")!
        )

        let result = try await client.updateSettings([])

        XCTAssertTrue(result.ok)
        XCTAssertTrue(result.characters.isEmpty)
        XCTAssertTrue(result.warnings.isEmpty)
    }

    func testBulkSettingsResponseDecodesCharactersAndWarnings() throws {
        let result = try JSONDecoder().decode(
            CharacterSettingsBulkResult.self,
            from: Data(
                #"{"ok":true,"characters":[{"id":"boss","name":"백부장","backend":"codex","model":"gpt-5.6-sol","effort":"ultra","fastMode":false,"permission":"danger-full-access","identityPrompt":"Lead"}],"warnings":["kept conversations"]}"#.utf8
            )
        )

        XCTAssertTrue(result.ok)
        XCTAssertEqual(result.characters.map(\.id), ["boss"])
        XCTAssertEqual(result.warnings, ["kept conversations"])
    }

    func testRuntimeCLIPathsRequestUsesAtomicLocalEndpointAndStringKeys() throws {
        let client = OfficeDatabaseClient(
            baseURL: URL(string: "http://127.0.0.1:4317")!
        )

        let request = try XCTUnwrap(client.runtimeCLIPathsRequest([
            .codex: " /Users/test/.volta/bin/codex ",
            .claude: "/Users/test/.asdf/shims/claude",
        ]))

        XCTAssertEqual(
            request.url?.absoluteString,
            "http://127.0.0.1:4317/api/runtime/cli-paths"
        )
        XCTAssertEqual(request.httpMethod, "PUT")
        XCTAssertEqual(
            request.value(forHTTPHeaderField: "Content-Type"),
            "application/json"
        )
        let body = try XCTUnwrap(request.httpBody)
        let object = try XCTUnwrap(
            JSONSerialization.jsonObject(with: body) as? [String: Any]
        )
        XCTAssertEqual(Set(object.keys), ["executables"])
        XCTAssertEqual(
            object["executables"] as? [String: String],
            [
                "codex": "/Users/test/.volta/bin/codex",
                "claude": "/Users/test/.asdf/shims/claude",
            ]
        )
    }

    func testEmptyRuntimeCLIPathsDoNotCreateARequest() throws {
        let client = OfficeDatabaseClient(
            baseURL: URL(string: "http://127.0.0.1:4317")!
        )

        XCTAssertNil(try client.runtimeCLIPathsRequest([:]))
        XCTAssertNil(try client.runtimeCLIPathsRequest([.codex: "  "]))
    }

    func testRuntimeCLIPathsResponseDecodesUpdatedCharacters() throws {
        let result = try JSONDecoder().decode(
            RuntimeCLIPathsResult.self,
            from: Data(
                #"{"ok":true,"updatedCharacterIds":["boss","left-man"]}"#.utf8
            )
        )

        XCTAssertTrue(result.ok)
        XCTAssertEqual(result.updatedCharacterIds, ["boss", "left-man"])
    }

    func testDockerProjectChoosesOnlyUnambiguousVolumeAutomatically() {
        XCTAssertEqual(
            OfficeDockerProject.selection(
                currentVolumeExists: false,
                legacyVolumeExists: false,
                preference: nil
            ),
            .current
        )
        XCTAssertEqual(
            OfficeDockerProject.selection(
                currentVolumeExists: true,
                legacyVolumeExists: false,
                preference: .legacy
            ),
            .current
        )
        XCTAssertEqual(
            OfficeDockerProject.selection(
                currentVolumeExists: false,
                legacyVolumeExists: true,
                preference: nil
            ),
            .legacyConfirmationRequired
        )
        XCTAssertEqual(
            OfficeDockerProject.selection(
                currentVolumeExists: false,
                legacyVolumeExists: true,
                preference: .current
            ),
            .legacyConfirmationRequired
        )
        XCTAssertEqual(
            OfficeDockerProject.selection(
                currentVolumeExists: false,
                legacyVolumeExists: true,
                preference: .legacy
            ),
            .legacy
        )
    }

    func testDockerProjectRequiresExplicitChoiceWhenBothVolumesExist() {
        XCTAssertEqual(
            OfficeDockerProject.selection(
                currentVolumeExists: true,
                legacyVolumeExists: true,
                preference: nil
            ),
            .projectChoiceRequired
        )
        XCTAssertEqual(
            OfficeDockerProject.selection(
                currentVolumeExists: true,
                legacyVolumeExists: true,
                preference: .current
            ),
            .current
        )
        XCTAssertEqual(
            OfficeDockerProject.selection(
                currentVolumeExists: true,
                legacyVolumeExists: true,
                preference: .legacy
            ),
            .legacy
        )
    }

    func testDockerProjectPreferencePersistsAndMigratesLegacyConfirmation() throws {
        let suiteName = "OfficeDockerProjectPreferenceTests-\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer {
            defaults.removePersistentDomain(forName: suiteName)
        }

        XCTAssertNil(OfficeDockerProjectPreference.load(from: defaults))

        defaults.set(true, forKey: "officeUseConfirmedLegacyDockerProject")
        XCTAssertEqual(
            OfficeDockerProjectPreference.load(from: defaults),
            .legacy
        )

        OfficeDockerProjectPreference.current.save(to: defaults)
        XCTAssertEqual(
            defaults.string(forKey: OfficeDockerProjectPreference.defaultsKey),
            OfficeDockerProjectPreference.current.rawValue
        )
        XCTAssertFalse(
            defaults.bool(forKey: "officeUseConfirmedLegacyDockerProject")
        )
        XCTAssertEqual(
            OfficeDockerProjectPreference.load(from: defaults),
            .current
        )

        OfficeDockerProjectPreference.legacy.save(to: defaults)
        XCTAssertEqual(
            OfficeDockerProjectPreference.load(from: defaults),
            .legacy
        )
    }

    func testSafeDiagnosticRedactsSensitiveLinesWithoutHidingNormalContext() {
        let output = """
        Docker daemon is ready
        Authorization: Bearer abc123
        API_TOKEN=super-secret-token
        db_password=hunter2
        clientSecret=value
        PostgreSQL failed to bind port 54329
        """

        let diagnostic = OfficeSetupAssistant.safeDiagnostic(output)

        XCTAssertTrue(diagnostic.contains("Docker daemon is ready"))
        XCTAssertTrue(diagnostic.contains("PostgreSQL failed to bind port 54329"))
        XCTAssertTrue(diagnostic.contains("[민감 정보가 포함된 줄 숨김]"))
        XCTAssertFalse(diagnostic.contains("abc123"))
        XCTAssertFalse(diagnostic.contains("super-secret-token"))
        XCTAssertFalse(diagnostic.contains("hunter2"))
        XCTAssertFalse(diagnostic.contains("clientSecret"))
    }

    func testSafeDiagnosticKeepsOnlyTheLastTwoThousandCharacters() {
        let output = String(repeating: "a", count: 300)
            + String(repeating: "b", count: 2_000)

        let diagnostic = OfficeSetupAssistant.safeDiagnostic(output)

        XCTAssertEqual(diagnostic.count, 2_000)
        XCTAssertEqual(diagnostic, String(repeating: "b", count: 2_000))
    }

    func testToolLocatorPrefersNewestNVMExecutableOverOtherHomePaths()
        throws
    {
        let local = root.appending(path: ".local/bin/claude")
        let olderNVM = root.appending(
            path: ".nvm/versions/node/v20.0.0/bin/claude"
        )
        let newerNVM = root.appending(
            path: ".nvm/versions/node/v24.0.0/bin/claude"
        )
        try createExecutable(at: local)
        try createExecutable(at: olderNVM)
        try createExecutable(at: newerNVM)

        let located = try XCTUnwrap(OfficeToolLocator.locate(
            "claude",
            homeDirectory: root
        ))
        XCTAssertEqual(
            located.resolvingSymlinksInPath().path,
            newerNVM.resolvingSymlinksInPath().path
        )
    }

    func testToolLocatorIgnoresNonExecutableCandidate() throws {
        let local = root.appending(path: ".local/bin/custom-cli")
        let volta = root.appending(path: ".volta/bin/custom-cli")
        try createFile(at: local)
        try createExecutable(at: volta)

        XCTAssertEqual(
            OfficeToolLocator.locate(
                "custom-cli",
                homeDirectory: root
            ),
            volta
        )
    }

    func testToolLocatorDoesNotTreatAnExecutableDirectoryAsACLI() throws {
        let name = "officestra-directory-only-cli"
        let directory = root.appending(path: ".local/bin/\(name)")
        try FileManager.default.createDirectory(
            at: directory,
            withIntermediateDirectories: true
        )
        try makeExecutable(directory)

        XCTAssertNil(OfficeToolLocator.locate(
            name,
            homeDirectory: root,
            searchPath: ""
        ))
    }

    func testDockerLocatorPrefersUserDockerCLI() throws {
        let docker = root.appending(path: ".docker/bin/docker")
        try createExecutable(at: docker)

        XCTAssertEqual(
            OfficeToolLocator.docker(homeDirectory: root),
            docker
        )
    }

    func testCLIProbePresentationSeparatesMissingLoginAndReady() {
        XCTAssertEqual(
            OfficeCLIInspector.check(
                for: OfficeCLIProbe(
                    backend: .codex,
                    executableURL: nil,
                    authenticated: false,
                    version: nil
                )
            ),
            .actionRequired("설치가 필요합니다.")
        )
        XCTAssertEqual(
            OfficeCLIInspector.check(
                for: OfficeCLIProbe(
                    backend: .claude,
                    executableURL: URL(fileURLWithPath: "/tmp/claude"),
                    authenticated: false,
                    version: "2.1"
                )
            ),
            .actionRequired("로그인이 필요합니다.")
        )
        XCTAssertEqual(
            OfficeCLIInspector.check(
                for: OfficeCLIProbe(
                    backend: .codex,
                    executableURL: URL(fileURLWithPath: "/tmp/codex"),
                    authenticated: true,
                    version: "codex-cli 1.0"
                )
            ),
            .ready("codex-cli 1.0")
        )
    }

    func testOnlyAuthenticatedCLIPathsArePreparedForSynchronization() {
        let paths = OfficeSetupAssistant.authenticatedExecutablePaths(from: [
            OfficeCLIProbe(
                backend: .codex,
                executableURL: URL(fileURLWithPath: "/valid/codex"),
                authenticated: true,
                version: "codex 1"
            ),
            OfficeCLIProbe(
                backend: .claude,
                executableURL: URL(fileURLWithPath: "/broken/claude"),
                authenticated: false,
                version: nil
            ),
        ])

        XCTAssertEqual(paths, [.codex: "/valid/codex"])
    }

    func testReadyStateStopsAtCLIPathFailureWithoutOfferingProviderRemap()
        async
    {
        var calls: [String] = []

        let issue = await OfficeSetupAssistant.evaluateReadyState(
            synchronizeCLIPaths: {
                calls.append("sync")
                throw CocoaError(.fileNoSuchFile)
            },
            compatibilityError: {
                calls.append("compatibility")
                return "should not run"
            }
        )

        XCTAssertEqual(calls, ["sync"])
        XCTAssertEqual(issue?.kind, .cliPathSynchronization)
        XCTAssertEqual(issue?.allowsProviderRemap, false)
    }

    func testReadyStateChecksCompatibilityOnlyAfterCLIPathSynchronization()
        async
    {
        var calls: [String] = []

        let issue = await OfficeSetupAssistant.evaluateReadyState(
            synchronizeCLIPaths: {
                calls.append("sync")
            },
            compatibilityError: {
                calls.append("compatibility")
                return "Claude login required"
            }
        )

        XCTAssertEqual(calls, ["sync", "compatibility"])
        XCTAssertEqual(issue?.kind, .providerCompatibility)
        XCTAssertEqual(issue?.message, "Claude login required")
        XCTAssertEqual(issue?.allowsProviderRemap, true)
    }

    func testReadyStateSucceedsAfterBothChecks() async {
        var calls: [String] = []

        let issue = await OfficeSetupAssistant.evaluateReadyState(
            synchronizeCLIPaths: {
                calls.append("sync")
            },
            compatibilityError: {
                calls.append("compatibility")
                return nil
            }
        )

        XCTAssertNil(issue)
        XCTAssertEqual(calls, ["sync", "compatibility"])
    }

    func testCodexOnlyRuntimeReassignsEveryCoworkerConsistently() {
        let configuration = sampleConfiguration()

        let runtime = configuration.preparingForRuntime(
            workdir: "/Users/example/project",
            availableBackends: [.codex],
            executablePaths: [.codex: "/Users/example/.local/bin/codex"]
        )

        XCTAssertEqual(runtime.workdir, "/Users/example/project")
        XCTAssertEqual(runtime.characters.map(\.backend), [.codex, .codex])
        XCTAssertEqual(
            runtime.characters.map(\.model),
            [AgentBackend.codex.defaultModel, AgentBackend.codex.defaultModel]
        )
        XCTAssertEqual(runtime.characters.map(\.effort), ["ultra", "max"])
        XCTAssertEqual(
            runtime.characters.map(\.permission),
            ["danger-full-access", "read-only"]
        )
        XCTAssertEqual(runtime.characters.map(\.fastMode), [true, false])
        XCTAssertEqual(
            runtime.characters.map(\.executablePath),
            [
                "/Users/example/.local/bin/codex",
                "/Users/example/.local/bin/codex",
            ]
        )
    }

    func testClaudeOnlyRuntimeRepairsUnsupportedEffortAndPermissionValues() {
        let configuration = sampleConfiguration()

        let runtime = configuration.preparingForRuntime(
            workdir: "/Users/example/project",
            availableBackends: [.claude],
            executablePaths: [.claude: "/Users/example/bin/claude"]
        )

        XCTAssertEqual(runtime.characters.map(\.backend), [.claude, .claude])
        XCTAssertEqual(
            runtime.characters.map(\.model),
            [
                AgentBackend.claude.defaultModel,
                AgentBackend.claude.defaultModel,
            ]
        )
        XCTAssertEqual(runtime.characters.map(\.effort), ["high", "max"])
        XCTAssertEqual(
            runtime.characters.map(\.permission),
            ["bypassPermissions", "plan"]
        )
        XCTAssertEqual(runtime.characters.map(\.fastMode), [true, false])
        XCTAssertEqual(
            runtime.characters.map(\.executablePath),
            ["/Users/example/bin/claude", "/Users/example/bin/claude"]
        )
    }

    func testBackendLaunchCommandExportsSearchPathAndRedirectsLogs() throws {
        let configuration = OfficeBackendLaunchConfiguration(
            workdir: URL(fileURLWithPath: "/Users/example/My Project"),
            backendDirectoryURL: URL(
                fileURLWithPath:
                    "/Applications/OFFICESTRA.app/Contents/Resources/OFFICESTRARuntime/backend"
            ),
            healthURL: URL(string: "http://127.0.0.1:4317/health")!,
            runtimeConfigurationURL: URL(
                fileURLWithPath:
                    "/Users/example/Library/Application Support/OFFICESTRA/runtime/characters.json"
            ),
            releaseID: "release-test",
            userID: 501
        )
        let node = URL(
            fileURLWithPath:
                "/Applications/OFFICESTRA.app/Contents/Resources/OFFICESTRARuntime/node/bin/node"
        )
        let logs = URL(
            fileURLWithPath:
                "/Users/example/Library/Application Support/OFFICESTRA/logs"
        )

        let launch = OfficeBackendLaunchCommands.start(
            configuration: configuration,
            nodeExecutableURL: node,
            standardOutputURL: logs.appending(path: "backend.out.log"),
            standardErrorURL: logs.appending(path: "backend.err.log"),
            executableSearchPaths: [
                "/Users/example/.nvm/versions/node/v24/bin",
                "/Users/example/.nvm/versions/node/v24/bin",
                "/custom/bin",
            ]
        )

        XCTAssertEqual(launch.executableURL.path, "/bin/launchctl")
        XCTAssertEqual(
            launch.arguments.prefix(5),
            ["submit", "-l", "com.neo.office-backend-4317", "--", "/bin/zsh"]
        )
        let command = try XCTUnwrap(launch.arguments.last)
        XCTAssertTrue(command.contains(
            "mkdir -p '\(logs.path)'"
        ))
        XCTAssertEqual(command.components(separatedBy: "mkdir -p").count - 1, 1)
        XCTAssertTrue(command.contains(
            "export PATH='\(node.deletingLastPathComponent().path):/Users/example/.nvm/versions/node/v24/bin:/custom/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin'"
        ))
        XCTAssertTrue(command.contains(
            "OFFICE_WORKDIR='/Users/example/My Project'"
        ))
        XCTAssertTrue(command.contains(
            "OFFICESTRA_RELEASE_ID='release-test'"
        ))
        XCTAssertTrue(command.contains(
            "CHARACTER_CONFIG_PATH='/Users/example/Library/Application Support/OFFICESTRA/runtime/characters.json'"
        ))
        XCTAssertTrue(command.contains(
            "exec '\(configuration.backendDirectoryURL.deletingLastPathComponent().appending(path: "rust/bin/officestra-backend").path)' >> '\(logs.path)/backend.out.log' 2>> '\(logs.path)/backend.err.log'"
        ))
    }

    func testBackendLaunchCommandQuotesApostrophesAsValidZsh() throws {
        let configuration = OfficeBackendLaunchConfiguration(
            workdir: URL(fileURLWithPath: "/Users/example/O'Brien Project"),
            backendDirectoryURL: URL(fileURLWithPath: "/tmp/backend"),
            healthURL: URL(string: "http://127.0.0.1:4317/health")!,
            runtimeConfigurationURL: URL(fileURLWithPath: "/tmp/config.json"),
            releaseID: "release-test",
            userID: 501
        )
        let launch = OfficeBackendLaunchCommands.start(
            configuration: configuration,
            nodeExecutableURL: URL(fileURLWithPath: "/tmp/node")
        )
        let command = try XCTUnwrap(launch.arguments.last)
        XCTAssertTrue(command.contains(
            "OFFICE_WORKDIR='/Users/example/O'\\''Brien Project'"
        ))

        let syntaxCheck = Process()
        syntaxCheck.executableURL = URL(fileURLWithPath: "/bin/zsh")
        syntaxCheck.arguments = ["-n", "-c", command]
        syntaxCheck.standardOutput = FileHandle.nullDevice
        syntaxCheck.standardError = FileHandle.nullDevice
        try syntaxCheck.run()
        syntaxCheck.waitUntilExit()
        XCTAssertEqual(syntaxCheck.terminationStatus, 0)
    }

    private func health(
        ok: Bool = true,
        service: String? = "officestra-backend",
        apiVersion: Int? = 1,
        workdir: String? = "/tmp/workspace",
        databaseOK: Bool = true,
        releaseID: String? = "release-test",
        activeTurnCount: Int? = 0,
        acceptingJobs: Bool? = nil,
        draining: Bool? = nil,
        idle: Bool? = nil
    ) -> OfficeBackendHealth {
        OfficeBackendHealth(
            ok: ok,
            service: service,
            apiVersion: apiVersion,
            pid: 123,
            releaseID: releaseID,
            activeTurnCount: activeTurnCount,
            acceptingJobs: acceptingJobs,
            draining: draining,
            idle: idle,
            workdir: workdir,
            database: .init(ok: databaseOK)
        )
    }

    private func sampleConfiguration() -> OfficeAgentConfiguration {
        OfficeAgentConfiguration(
            workdir: "/configured",
            databaseBaseURL: URL(string: "http://127.0.0.1:4317")!,
            archiveCabinetHitbox: hitbox,
            characters: [
                CharacterConfiguration(
                    id: .boss,
                    name: "Boss",
                    seat: "top",
                    backend: .codex,
                    identityPrompt: "Lead",
                    model: "gpt-5.6-terra",
                    effort: "ultra",
                    fastMode: true,
                    permission: "danger-full-access",
                    executablePath: "/old/codex",
                    hitbox: hitbox,
                    monitorHitbox: hitbox,
                    bubble: bubble
                ),
                CharacterConfiguration(
                    id: .leftMan,
                    name: "Claude",
                    seat: "left",
                    backend: .claude,
                    identityPrompt: "Review",
                    model: "claude-sonnet-5",
                    effort: "max",
                    fastMode: false,
                    permission: "plan",
                    executablePath: "/old/claude",
                    hitbox: hitbox,
                    monitorHitbox: hitbox,
                    bubble: bubble
                ),
            ]
        )
    }

    private func storedCharacter(
        id: String,
        backend: AgentBackend,
        effort: String? = nil,
        fastMode: Bool = false,
        permission: String? = nil
    ) -> StoredCharacterProfile {
        StoredCharacterProfile(
            id: id,
            name: id,
            backend: backend,
            model: backend.defaultModel,
            effort: effort ?? backend.effortOptions[0],
            fastMode: fastMode,
            permission: permission
                ?? (backend == .codex ? "read-only" : "plan"),
            identityPrompt: ""
        )
    }

    private var hitbox: CharacterHitbox {
        CharacterHitbox(x: 0, y: 0, width: 10, height: 10)
    }

    private var bubble: CharacterBubbleAnchor {
        CharacterBubbleAnchor(x: 1, y: 1)
    }

    private func createFile(at url: URL) throws {
        try FileManager.default.createDirectory(
            at: url.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        XCTAssertTrue(FileManager.default.createFile(
            atPath: url.path,
            contents: Data()
        ))
    }

    private func createExecutable(at url: URL) throws {
        try createFile(at: url)
        try makeExecutable(url)
    }

    private func makeExecutable(_ url: URL) throws {
        try FileManager.default.setAttributes(
            [.posixPermissions: NSNumber(value: 0o755)],
            ofItemAtPath: url.path
        )
    }
}
