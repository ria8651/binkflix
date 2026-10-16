-- Device pairing moved to bastion's device sign-in: the Apple TV app now
-- authenticates with the bastion JWT itself, so binkflix holds no device
-- credentials of its own.
DROP TABLE IF EXISTS device_tokens;
