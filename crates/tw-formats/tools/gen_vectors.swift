import Foundation

// Same rules as TypeWhisper/Services/Sync/UserDataSyncIdentity.swift (Mac).
func normalizedKey(_ value: String) -> String {
    value.trimmingCharacters(in: .whitespacesAndNewlines)
        .folding(options: [.caseInsensitive, .diacriticInsensitive], locale: Locale(identifier: "en_US_POSIX"))
        .precomposedStringWithCanonicalMapping
}
func encodedKey(_ value: String) -> String {
    Data(value.utf8).base64EncodedString()
        .replacingOccurrences(of: "+", with: "-").replacingOccurrences(of: "/", with: "_").replacingOccurrences(of: "=", with: "")
}
let inputs = ["  Recieve ", "Café", "Cafe\u{301}", "STRASSE", "Straße", "İstanbul", "ı", "ﬁle", "Ångström", "ÆON", "Øresund", "naïve", "日本語", "Ǆemal", "ŉ", "ΣΊΣΥΦΟΣ", "\u{00A0}tab\t\n", "Ⅻ", "ｆｕｌｌ", "e\u{0308}\u{0301}", "Œuvre", "ǅ", "K", "Å", "ﬀ", "ǰ", "Mötley Crüe", ";hello", "🙂 Emoji"]
var rows: [[String: String]] = []
for s in inputs {
    let n = normalizedKey(s)
    rows.append(["input": s, "normalized": n, "termId": "dictionary:term:\(encodedKey(n))", "snippetId": "snippet:\(encodedKey(n))", "regexId": "dictionary:correction:regex:\(encodedKey(s))"])
}
let data = try JSONSerialization.data(withJSONObject: rows, options: [.prettyPrinted, .sortedKeys])
FileManager.default.createFile(atPath: "tests/fixtures/identity-vectors.json", contents: data)
print("wrote \(rows.count) vectors")
