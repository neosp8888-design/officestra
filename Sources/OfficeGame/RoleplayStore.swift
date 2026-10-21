// 직원별 롤플레이 대화 설정·기록을 백엔드에서 읽고 바꾸는 저장소.
import Foundation
import OfficeCore

struct RoleplayAsset: Codable, Identifiable, Equatable {
    let id: String
    let kind: String
    let name: String
    let promptCount: Int
    let regexCount: Int
    let toggleCount: Int
}

struct RoleplayProfile: Codable, Equatable {
    var characterId: String
    var enabled: Bool
    var presetId: String?
    var moduleIds: [String]
    var toggles: [String: String]
    var characterDescription: String
    var firstMessage: String
    var userName: String
    var persona: String
    var model: String
    var localProfileId: String?
    var currentChatId: String?
}

struct RoleplayToggleDefinition: Codable, Equatable {
    let key: String
    let value: String
    let type: String
    let options: [String]
}

struct RoleplayMessage: Codable, Identifiable, Equatable {
    let id: String
    let role: String
    let content: String
}

struct RoleplayState: Codable, Equatable {
    var profile: RoleplayProfile
    var toggleDefinitions: [RoleplayToggleDefinition]
    var activeModuleIds: [String]
    var messages: [RoleplayMessage]
    var generating: Bool
    var lastError: String?
}

/// 보낼 항목만 채운다. 비어 있는 항목은 JSON에서 빠져 기존 값을 유지한다.
struct RoleplayProfilePatch: Encodable {
    var enabled: Bool?
    var presetId: String?
    var moduleIds: [String]?
    var toggles: [String: String]?
    var characterDescription: String?
    var firstMessage: String?
    var userName: String?
    var persona: String?
    var model: String?
    /// 빈 문자열을 보내면 안티그래비티 프록시로 되돌린다.
    var localProfileId: String?
}

private struct RoleplayAssetsResponse: Decodable { let assets: [RoleplayAsset] }
private struct RoleplayImportResponse: Decodable { let asset: RoleplayAsset }
private struct RoleplayErrorResponse: Decodable { let error: String }
private struct RoleplayAssetUpload: Encodable { let fileName: String; let dataBase64: String }
private struct RoleplayMessageInput: Encodable { let content: String }

private enum RoleplayClientError: LocalizedError {
    case message(String)

    var errorDescription: String? {
        if case .message(let message) = self { return message }
        return nil
    }
}

struct RoleplayClient: Sendable {
    let baseURL: URL

    func assets() async throws -> [RoleplayAsset] {
        let (data, response) = try await URLSession.shared.data(from: baseURL.appending(path: "api/roleplay/assets"))
        try validate(response, data: data)
        return try JSONDecoder().decode(RoleplayAssetsResponse.self, from: data).assets
    }

    func importAsset(fileName: String, data fileData: Data) async throws -> RoleplayAsset {
        let body = RoleplayAssetUpload(fileName: fileName, dataBase64: fileData.base64EncodedString())
        let data = try await send("POST", "api/roleplay/assets", body: body)
        return try JSONDecoder().decode(RoleplayImportResponse.self, from: data).asset
    }

    func state(_ character: OfficeCharacter) async throws -> RoleplayState {
        let (data, response) = try await URLSession.shared.data(from: profileURL(character))
        try validate(response, data: data)
        return try JSONDecoder().decode(RoleplayState.self, from: data)
    }

    func update(_ character: OfficeCharacter, _ patch: RoleplayProfilePatch) async throws -> RoleplayState {
        let data = try await send("PUT", "api/roleplay/profiles/\(character.rawValue)", body: patch)
        return try JSONDecoder().decode(RoleplayState.self, from: data)
    }

    func newChat(_ character: OfficeCharacter) async throws -> RoleplayState {
        let data = try await send("POST", "api/roleplay/profiles/\(character.rawValue)/chats", body: [String: String]())
        return try JSONDecoder().decode(RoleplayState.self, from: data)
    }

    func sendMessage(_ content: String, to character: OfficeCharacter) async throws {
        _ = try await send(
            "POST",
            "api/roleplay/profiles/\(character.rawValue)/messages",
            body: RoleplayMessageInput(content: content)
        )
    }

    private func profileURL(_ character: OfficeCharacter) -> URL {
        baseURL.appending(path: "api/roleplay/profiles/\(character.rawValue)")
    }

    private func send(_ method: String, _ path: String, body: some Encodable) async throws -> Data {
        var request = URLRequest(url: baseURL.appending(path: path))
        request.httpMethod = method
        request.setValue("application/json", forHTTPHeaderField: "content-type")
        request.httpBody = try JSONEncoder().encode(body)
        let (data, response) = try await URLSession.shared.data(for: request)
        try validate(response, data: data)
        return data
    }

    private func validate(_ response: URLResponse, data: Data) throws {
        guard let response = response as? HTTPURLResponse else {
            throw RoleplayClientError.message("백엔드 응답을 확인할 수 없습니다.")
        }
        guard (200..<300).contains(response.statusCode) else {
            let message = try? JSONDecoder().decode(RoleplayErrorResponse.self, from: data).error
            throw RoleplayClientError.message(
                message ?? (response.statusCode == 404
                    ? "롤플레이 API를 사용할 수 없습니다. 백엔드를 앱 버튼으로 재시작해 주세요."
                    : "롤플레이 요청 실패 (HTTP \(response.statusCode))")
            )
        }
    }
}

@MainActor
final class RoleplayStore: ObservableObject {
    @Published private(set) var states: [OfficeCharacter: RoleplayState] = [:]
    @Published private(set) var assets: [RoleplayAsset] = []
    /// 화면 조작 중 생긴 오류. 모델 생성 실패는 RoleplayState.lastError로 따로 온다.
    @Published var errors: [OfficeCharacter: String] = [:]
    @Published var assetError: String?

    private let client: RoleplayClient

    init(baseURL: URL) {
        client = RoleplayClient(baseURL: baseURL)
    }

    func isEnabled(_ character: OfficeCharacter) -> Bool {
        states[character]?.profile.enabled == true
    }

    func refresh(_ character: OfficeCharacter) async {
        do {
            let state = try await client.state(character)
            states[character] = state
            // 다른 곳에서 가져온 프리셋·모듈이면 목록부터 다시 읽어야 이름이 보인다.
            let known = Set(assets.map(\.id))
            if !Set([state.profile.presetId].compactMap { $0 } + state.activeModuleIds).isSubset(of: known) {
                await refreshAssets()
            }
        } catch {
            errors[character] = error.localizedDescription
        }
    }

    func refresh(_ characters: [OfficeCharacter]) async {
        for character in characters { await refresh(character) }
    }

    func refreshAssets() async {
        do {
            assets = try await client.assets()
            assetError = nil
        } catch {
            assetError = error.localizedDescription
        }
    }

    func update(_ character: OfficeCharacter, _ patch: RoleplayProfilePatch) async {
        do {
            states[character] = try await client.update(character, patch)
            errors[character] = nil
        } catch {
            errors[character] = error.localizedDescription
        }
    }

    func setEnabled(_ enabled: Bool, for character: OfficeCharacter) async {
        await update(character, RoleplayProfilePatch(enabled: enabled))
        if enabled { await refreshAssets() }
    }

    func setToggle(_ key: String, value: String, for character: OfficeCharacter) async {
        guard var toggles = states[character]?.profile.toggles else { return }
        toggles[key] = value
        await update(character, RoleplayProfilePatch(toggles: toggles))
    }

    func newChat(_ character: OfficeCharacter) async {
        do {
            states[character] = try await client.newChat(character)
            errors[character] = nil
        } catch {
            errors[character] = error.localizedDescription
        }
    }

    func importAsset(from url: URL) async -> RoleplayAsset? {
        do {
            let data = try Data(contentsOf: url)
            let asset = try await client.importAsset(fileName: url.lastPathComponent, data: data)
            await refreshAssets()
            return asset
        } catch {
            assetError = error.localizedDescription
            return nil
        }
    }

    /// 입력창 제출용. 이미 답을 만드는 중이면 받지 않는다.
    func send(_ text: String, for character: OfficeCharacter) -> Bool {
        let content = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !content.isEmpty, states[character]?.generating != true else { return false }
        states[character]?.generating = true
        errors[character] = nil
        Task {
            do {
                try await client.sendMessage(content, to: character)
            } catch {
                errors[character] = error.localizedDescription
            }
            await refresh(character)
        }
        return true
    }
}
