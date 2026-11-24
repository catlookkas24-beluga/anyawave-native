# AnyaWave Native (Rust + C++)
# Update summary 24/11/26

# 1.DAVE (E2EE) is done, but untested

	•	Upgraded davey from 0.1 to 0.2. Version 0.1 switched keys immediately after a commit/welcome, which violates the protocol (it has to wait for op22), so audio could break up during epoch changes.
	•	Rewrote dave.rs to use prepare_transition / execute_transition.
	•	connection.rs: fixed the op21–31 ordering, added handling for op11/op13, removed the misplaced ready message in op24, and a failed commit now sends op31 and restarts the handshake.
	•	gateway.rs: added opcodes 11 and 13.

# 2.Password system that never stores plaintext

	•	New file auth.rs: the server stores only HMAC-SHA256(pepper, password) and compares it in constant time.
	•	Removed the youshallnotpass default. If the variables aren't set, the server refuses to start.
	•	Passwords must be at least 20 characters.
	•	Generate the value with aw-server --make-digest.
	•	The full main.rs now uses this system, including /hifi/stream and /debug/*.
