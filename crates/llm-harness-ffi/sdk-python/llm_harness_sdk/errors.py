import json


class HarnessError(RuntimeError):
    @classmethod
    def from_error_json(cls, text):
        if not text:
            return cls("native harness error")
        try:
            payload = json.loads(text)
        except json.JSONDecodeError:
            return cls(text)

        message = payload.get("message") or text
        err = cls(message)
        err.code = payload.get("code")
        err.kind = payload.get("kind")
        err.payload = payload
        return err
