#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("ax_visible_character_range is supported only on macOS");
    std::process::exit(2);
}

#[cfg(target_os = "macos")]
fn main() {
    if let Err(error) = macos::run() {
        eprintln!("AX probe failed: {error}");
        eprintln!(
            "Grant the invoking terminal Accessibility permission in System Settings > Privacy & Security > Accessibility, then retry."
        );
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::error::Error;
    use std::fmt;
    use std::ptr::{NonNull, null};

    use objc2_application_services::{AXError, AXUIElement, AXValue, AXValueType};
    use objc2_core_foundation::{CFArray, CFNumber, CFRange, CFRetained, CFString, CFType};

    const AX_ROLE: &str = "AXRole";
    const AX_CHILDREN: &str = "AXChildren";
    const AX_WINDOWS: &str = "AXWindows";
    const AX_NUMBER_OF_CHARACTERS: &str = "AXNumberOfCharacters";
    const AX_SELECTED_TEXT_RANGE: &str = "AXSelectedTextRange";
    const AX_VISIBLE_CHARACTER_RANGE: &str = "AXVisibleCharacterRange";

    #[derive(Debug)]
    struct ProbeError(String);

    impl fmt::Display for ProbeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(&self.0)
        }
    }

    impl Error for ProbeError {}

    pub(super) fn run() -> Result<(), Box<dyn Error>> {
        let pid = std::env::args()
            .nth(1)
            .ok_or_else(|| ProbeError("usage: ax_visible_character_range <PID>".into()))?
            .parse::<i32>()
            .map_err(|error| ProbeError(format!("invalid PID: {error}")))?;
        let application = unsafe { AXUIElement::new_application(pid) };
        let window = elements(&application, AX_WINDOWS)?
            .into_iter()
            .find(|element| role(element).is_ok_and(|role| role == "AXWindow"))
            .ok_or_else(|| ProbeError("no AXWindow found for the requested process".into()))?;
        let terminal = find_role(&window, "AXTextArea", 0)?
            .ok_or_else(|| ProbeError("no terminal AXTextArea found below AXWindow".into()))?;

        println!("window role: {}", role(&window)?);
        println!("terminal role: {}", role(&terminal)?);
        println!("AXNumberOfCharacters: {}", number(&terminal, AX_NUMBER_OF_CHARACTERS)?);
        let selected = range(&terminal, AX_SELECTED_TEXT_RANGE)?;
        println!("AXSelectedTextRange: location={}, length={}", selected.location, selected.length);
        let visible = range(&terminal, AX_VISIBLE_CHARACTER_RANGE)?;
        println!(
            "AXVisibleCharacterRange: location={}, length={}",
            visible.location, visible.length
        );
        Ok(())
    }

    fn find_role(
        element: &AXUIElement,
        expected: &str,
        depth: usize,
    ) -> Result<Option<CFRetained<AXUIElement>>, Box<dyn Error>> {
        if depth > 64 {
            return Ok(None);
        }
        for child in elements(element, AX_CHILDREN)? {
            if role(&child).is_ok_and(|role| role == expected) {
                return Ok(Some(child));
            }
            if let Some(found) = find_role(&child, expected, depth + 1)? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    fn role(element: &AXUIElement) -> Result<String, Box<dyn Error>> {
        copy_attribute(element, AX_ROLE)?
            .downcast::<CFString>()
            .map(|role| role.to_string())
            .map_err(|_| ProbeError("AXRole was not a string".into()).into())
    }

    fn number(element: &AXUIElement, attribute: &str) -> Result<i64, Box<dyn Error>> {
        copy_attribute(element, attribute)?
            .downcast::<CFNumber>()
            .map_err(|_| ProbeError(format!("{attribute} was not a number")))?
            .as_i64()
            .ok_or_else(|| ProbeError(format!("{attribute} could not be converted to i64")).into())
    }

    fn range(element: &AXUIElement, attribute: &str) -> Result<CFRange, Box<dyn Error>> {
        let value = copy_attribute(element, attribute)?
            .downcast::<AXValue>()
            .map_err(|_| ProbeError(format!("{attribute} was not an AXValue")))?;
        let mut range = CFRange::new(0, 0);
        let pointer = NonNull::from(&mut range).cast();
        if unsafe { value.value(AXValueType::CFRange, pointer) } {
            Ok(range)
        } else {
            Err(ProbeError(format!("{attribute} was not a CFRange AXValue")).into())
        }
    }

    fn elements(
        element: &AXUIElement,
        attribute: &str,
    ) -> Result<Vec<CFRetained<AXUIElement>>, Box<dyn Error>> {
        let array = copy_attribute(element, attribute)?
            .downcast::<CFArray>()
            .map_err(|_| ProbeError(format!("{attribute} was not an array")))?;
        let array = unsafe { CFRetained::cast_unchecked::<CFArray<CFType>>(array) };
        Ok(array
            .to_vec()
            .into_iter()
            .filter_map(|value| value.downcast::<AXUIElement>().ok())
            .collect())
    }

    fn copy_attribute(
        element: &AXUIElement,
        attribute: &str,
    ) -> Result<CFRetained<CFType>, Box<dyn Error>> {
        let name = CFString::from_str(attribute);
        let mut value: *const CFType = null();
        let error = unsafe { element.copy_attribute_value(&name, NonNull::from(&mut value)) };
        if error != AXError::Success {
            return Err(ProbeError(format!("{attribute} query returned {error:?}")).into());
        }
        let value = NonNull::new(value.cast_mut())
            .ok_or_else(|| ProbeError(format!("{attribute} returned no value")))?;
        Ok(unsafe { CFRetained::from_raw(value) })
    }
}
