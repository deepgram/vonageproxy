use base64;
use failure::Error;

#[derive(Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct BasicAuthentication {
    pub username: String,
    pub password: String,
}

impl BasicAuthentication {
    /// Takes the value of the 'Authorization' header and tries to construct a
    /// BasicAuthentication object from it.
    pub fn parse(header: &str) -> Result<Self, Error> {
        let parts: Vec<_> = header.splitn(2, ' ').collect();
        ensure!(parts.len() == 2, "Invalid Authentication header.");
        ensure!(
            parts[0] == "Basic",
            "Authentication header is not a supported type."
        );
        let bytes = base64::decode(&parts[1])?;
        let decoded = String::from_utf8(bytes)?;
        let parts: Vec<_> = decoded.splitn(2, ':').collect();
        ensure!(parts.len() == 2, "Invalid basic authentication header.");
        Ok(Self {
            username: parts[0].to_string(),
            password: parts[1].to_string(),
        })
    }

    pub fn to_string(&self) -> String {
        format!(
            "Basic {}",
            base64::encode(&format!("{}:{}", self.username, self.password))
        )
    }
}

impl<T, U> From<(T, U)> for BasicAuthentication
where
    T: AsRef<str>,
    U: AsRef<str>,
{
    fn from(other: (T, U)) -> Self {
        Self {
            username: other.0.as_ref().to_string(),
            password: other.1.as_ref().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_auth() {
        assert_eq!(
            BasicAuthentication::parse("Basic dXNlcjpwYXNz").unwrap(),
            ("user", "pass").into()
        );

        assert_eq!(
            BasicAuthentication::from(("user", "pass")).to_string(),
            "Basic dXNlcjpwYXNz"
        )
    }
}
